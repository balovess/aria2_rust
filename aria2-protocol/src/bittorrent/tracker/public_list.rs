use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use tokio::sync::{Mutex, Notify, watch};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::http::client::ensure_ring_provider;

mod parse;

#[cfg(test)]
mod tests;

use parse::EMBEDDED_TRACKER_LIST;

pub const DEFAULT_TRACKER_SOURCE: &str = "https://cf.trackerslist.com/best.txt";
pub const DEFAULT_TRACKER_UPDATE_INTERVAL: Duration = Duration::from_secs(604_800);
pub const DEFAULT_TRACKER_SOURCE_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound source downloads so a large user-provided source list cannot create
/// an unbounded burst of outbound HTTP requests.
pub const MAX_TRACKER_SOURCE_CONCURRENCY: usize = 4;
const MAX_BACKOFF: Duration = Duration::from_secs(3_600);
const REMOTE_TEMPORARY_BACKOFF: Duration = Duration::from_secs(30);

/// Fetches the body of a configured public tracker-list source.
///
/// The catalog owns source scheduling and parsing. Callers embedding it in an
/// engine can provide the transport so process network policy is applied at
/// the layer that owns that policy.
pub trait TrackerSourceFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        source: &'a str,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerProtocol {
    Http,
    Https,
    Udp,
    Wss,
}

/// Failure categories used to keep local connectivity problems out of the
/// process-wide public tracker health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerFailureKind {
    /// DNS, socket, proxy, or TLS failures where the tracker was not reached.
    Network,
    /// The request or response timed out without a tracker-level response.
    Timeout,
    /// The tracker responded with a temporary server-side status.
    RemoteTemporary,
    /// The tracker explicitly rejected the announce.
    TrackerRejected,
    /// The remote response was not a valid tracker response.
    MalformedResponse,
}

impl TrackerFailureKind {
    /// Stable lowercase name used by RPC and diagnostic output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::RemoteTemporary => "remoteTemporary",
            Self::TrackerRejected => "trackerRejected",
            Self::MalformedResponse => "malformedResponse",
        }
    }
}

impl TrackerProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Udp => "udp",
            Self::Wss => "wss",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerEntry {
    pub url: String,
    pub protocol: TrackerProtocol,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerCatalogConfig {
    pub enabled: bool,
    pub sources: Vec<String>,
    pub update_interval: Duration,
}

impl Default for TrackerCatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sources: vec![DEFAULT_TRACKER_SOURCE.to_string()],
            update_interval: DEFAULT_TRACKER_UPDATE_INTERVAL,
        }
    }
}

pub struct PublicTrackerListStats {
    pub total_entries: usize,
    pub http_count: usize,
    pub udp_count: usize,
    pub wss_count: usize,
    pub is_embedded_fallback: bool,
    pub last_updated: Option<Duration>,
    pub sources: Vec<String>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Default)]
struct TrackerHealth {
    consecutive_failures: u32,
    next_retry: Option<Instant>,
    successes: u64,
    failures: u64,
}

pub struct PublicTrackerList {
    entries: tokio::sync::RwLock<Arc<Vec<TrackerEntry>>>,
    source_entries: tokio::sync::RwLock<HashMap<String, Vec<TrackerEntry>>>,
    source_fetcher: tokio::sync::RwLock<Arc<dyn TrackerSourceFetcher>>,
    config: tokio::sync::RwLock<TrackerCatalogConfig>,
    source_request_timeout: tokio::sync::RwLock<Duration>,
    health: Mutex<HashMap<String, TrackerHealth>>,
    last_updated: tokio::sync::RwLock<Option<Instant>>,
    last_refresh_error: tokio::sync::RwLock<Option<String>>,
    running: AtomicBool,
    update_started: AtomicBool,
    config_changed: Notify,
    updates: watch::Sender<u64>,
}

impl Default for PublicTrackerList {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicTrackerList {
    pub fn new() -> Self {
        Self::new_with_config(TrackerCatalogConfig::default())
    }

    pub fn new_with_config(config: TrackerCatalogConfig) -> Self {
        Self::new_with_config_and_fetcher(config, Arc::new(ReqwestTrackerSourceFetcher::new()))
    }

    /// Construct a catalog using a caller-owned HTTP source transport.
    pub fn new_with_config_and_fetcher(
        config: TrackerCatalogConfig,
        source_fetcher: Arc<dyn TrackerSourceFetcher>,
    ) -> Self {
        let entries = Self::parse(EMBEDDED_TRACKER_LIST);
        let (updates, _) = watch::channel(0);
        Self {
            entries: tokio::sync::RwLock::new(Arc::new(entries)),
            source_entries: tokio::sync::RwLock::new(HashMap::new()),
            source_fetcher: tokio::sync::RwLock::new(source_fetcher),
            config: tokio::sync::RwLock::new(config),
            source_request_timeout: tokio::sync::RwLock::new(DEFAULT_TRACKER_SOURCE_TIMEOUT),
            health: Mutex::new(HashMap::new()),
            last_updated: tokio::sync::RwLock::new(Some(Instant::now())),
            last_refresh_error: tokio::sync::RwLock::new(None),
            running: AtomicBool::new(true),
            update_started: AtomicBool::new(false),
            config_changed: Notify::new(),
            updates,
        }
    }

    /// Subscribe to tracker catalog membership and configuration changes.
    /// Health/backoff changes are intentionally excluded; callers can query
    /// the current availability snapshot when this revision advances.
    pub fn subscribe_updates(&self) -> watch::Receiver<u64> {
        self.updates.subscribe()
    }

    fn notify_updated(&self) {
        self.updates
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub fn parse(text: &str) -> Vec<TrackerEntry> {
        parse::parse_tracker_list(text)
    }

    pub async fn config(&self) -> TrackerCatalogConfig {
        self.config.read().await.clone()
    }

    /// Return the timeout applied to each complete tracker-source HTTP fetch.
    pub async fn source_request_timeout(&self) -> Duration {
        *self.source_request_timeout.read().await
    }

    /// Set the timeout for future tracker-source fetches.
    ///
    /// An in-progress fetch keeps the timeout captured when it started. A
    /// zero duration restores the library default.
    pub async fn set_source_request_timeout(&self, timeout: Duration) {
        *self.source_request_timeout.write().await = if timeout.is_zero() {
            DEFAULT_TRACKER_SOURCE_TIMEOUT
        } else {
            timeout
        };
        self.notify_updated();
        self.config_changed.notify_one();
    }

    /// Set the source-fetch timeout synchronously before the update loop starts.
    pub fn set_source_request_timeout_now(&self, timeout: Duration) {
        *self
            .source_request_timeout
            .try_write()
            .expect("tracker source timeout must not be held during engine setup") =
            if timeout.is_zero() {
                DEFAULT_TRACKER_SOURCE_TIMEOUT
            } else {
                timeout
            };
        self.notify_updated();
        self.config_changed.notify_one();
    }

    pub async fn set_config(&self, mut config: TrackerCatalogConfig) {
        config.sources = normalize_sources(config.sources);
        if config.update_interval.is_zero() {
            config.update_interval = DEFAULT_TRACKER_UPDATE_INTERVAL;
        }
        *self.config.write().await = config;
        self.notify_updated();
        self.config_changed.notify_one();
    }

    /// Apply configuration synchronously before the engine starts.
    ///
    /// Engine construction happens before its event loop is spawned, so using
    /// a blocking task here would introduce a race with the first download.
    pub fn set_config_now(&self, mut config: TrackerCatalogConfig) {
        config.sources = normalize_sources(config.sources);
        if config.update_interval.is_zero() {
            config.update_interval = DEFAULT_TRACKER_UPDATE_INTERVAL;
        }
        *self
            .config
            .try_write()
            .expect("public tracker config must not be held during engine setup") = config;
        self.notify_updated();
        self.config_changed.notify_one();
    }

    pub async fn sources(&self) -> Vec<String> {
        self.config.read().await.sources.clone()
    }

    pub async fn contains(&self, url: &str) -> bool {
        self.snapshot().await.iter().any(|entry| entry.url == url)
    }

    pub async fn snapshot(&self) -> Arc<Vec<TrackerEntry>> {
        self.entries.read().await.clone()
    }

    pub async fn available_snapshot(&self) -> Arc<Vec<TrackerEntry>> {
        if !self.config.read().await.enabled {
            return Arc::new(Vec::new());
        }
        let entries = self.snapshot().await;
        let now = Instant::now();
        let health = self.health.lock().await;
        Arc::new(
            entries
                .iter()
                .filter(|entry| {
                    health
                        .get(&entry.url)
                        .and_then(|state| state.next_retry)
                        .is_none_or(|retry_at| retry_at <= now)
                })
                .cloned()
                .collect(),
        )
    }

    pub async fn get_http_trackers(&self) -> Vec<String> {
        let entries = self.available_snapshot().await;
        entries
            .iter()
            .filter(|e| e.protocol == TrackerProtocol::Http || e.protocol == TrackerProtocol::Https)
            .map(|e| e.url.clone())
            .collect()
    }

    pub async fn get_udp_trackers(&self) -> Vec<String> {
        let entries = self.available_snapshot().await;
        entries
            .iter()
            .filter(|e| e.protocol == TrackerProtocol::Udp)
            .map(|e| e.url.clone())
            .collect()
    }

    pub async fn get_all(&self) -> Vec<TrackerEntry> {
        self.snapshot().await.as_ref().clone()
    }

    pub async fn fetch_and_update(&self, url: &str) -> Result<usize, String> {
        let timeout = self.source_request_timeout().await;
        let fetcher = self.source_fetcher.read().await.clone();
        let entries = fetch_tracker_source(fetcher.as_ref(), url, timeout).await?;
        self.source_entries
            .write()
            .await
            .insert(url.to_string(), entries);
        self.merge_source_snapshots().await
    }

    pub fn start_auto_update(self: &Arc<Self>, url: String, interval: Duration) {
        self.set_config_now(TrackerCatalogConfig {
            enabled: true,
            sources: vec![url],
            update_interval: interval,
        });
        self.start_catalog_update();
    }

    pub fn start_catalog_update(self: &Arc<Self>) {
        if self.update_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let catalog = Arc::clone(self);
        tokio::spawn(async move {
            while catalog.running.load(Ordering::Relaxed) {
                let config = catalog.config().await;
                if config.enabled
                    && let Err(error) = catalog.refresh().await
                {
                    warn!("Public tracker catalog refresh failed: {}", error);
                }
                let sleep = sleep(config.update_interval);
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {},
                    _ = catalog.config_changed.notified() => {},
                }
            }
            info!("Public tracker catalog update loop exited");
        });
    }

    pub async fn refresh(&self) -> Result<usize, String> {
        let config = self.config().await;
        if !config.enabled {
            return Ok(0);
        }
        let sources = normalize_sources(config.sources);
        if sources.is_empty() {
            *self.last_refresh_error.write().await = None;
            return Ok(0);
        }

        let fetcher = self.source_fetcher.read().await.clone();
        let request_timeout = self.source_request_timeout().await;
        let results = stream::iter(sources.iter().cloned().map(|source| {
            let fetcher = Arc::clone(&fetcher);
            async move {
                let result = fetch_tracker_source(fetcher.as_ref(), &source, request_timeout).await;
                (source, result)
            }
        }))
        .buffer_unordered(MAX_TRACKER_SOURCE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        let mut errors = Vec::new();
        let mut source_entries = self.source_entries.write().await;
        source_entries.retain(|source, _| sources.contains(source));
        for (source, result) in results {
            match result {
                Ok(entries) => {
                    source_entries.insert(source, entries);
                }
                Err(error) => errors.push(format!("{}: {}", source, error)),
            }
        }
        let has_source_snapshot = !source_entries.is_empty();
        drop(source_entries);

        if !has_source_snapshot {
            let error = errors.join("; ");
            *self.last_refresh_error.write().await = Some(error.clone());
            return Err(if error.is_empty() {
                "All public tracker sources returned no valid trackers".to_string()
            } else {
                error
            });
        }

        let count = self.merge_source_snapshots().await?;
        *self.last_refresh_error.write().await = (!errors.is_empty()).then_some(errors.join("; "));
        Ok(count)
    }

    async fn merge_source_snapshots(&self) -> Result<usize, String> {
        let source_entries = self.source_entries.read().await;
        let mut merged = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entries in source_entries.values() {
            for entry in entries {
                if seen.insert(entry.url.clone()) {
                    merged.push(entry.clone());
                }
            }
        }
        drop(source_entries);
        self.replace_snapshot(merged).await
    }

    async fn replace_snapshot(&self, entries: Vec<TrackerEntry>) -> Result<usize, String> {
        if entries.is_empty() {
            return Err("Parsed 0 entries from response".to_string());
        }
        let count = entries.len();
        *self.entries.write().await = Arc::new(entries);
        let current_urls: std::collections::HashSet<_> = self
            .entries
            .read()
            .await
            .iter()
            .map(|entry| entry.url.clone())
            .collect();
        self.health
            .lock()
            .await
            .retain(|url, _| current_urls.contains(url));
        *self.last_updated.write().await = Some(Instant::now());
        self.notify_updated();
        info!("Public tracker catalog updated: {} trackers", count);
        Ok(count)
    }

    pub async fn record_success(&self, url: &str) {
        let mut health = self.health.lock().await;
        let state = health.entry(url.to_string()).or_default();
        state.consecutive_failures = 0;
        state.next_retry = None;
        state.successes = state.successes.saturating_add(1);
    }

    pub async fn record_failure(&self, url: &str) {
        self.record_failure_kind(url, TrackerFailureKind::RemoteTemporary)
            .await;
    }

    /// Record a classified announce failure.
    ///
    /// Local failures are deliberately telemetry-only at catalog scope. A
    /// caller's DNS outage, offline connection, or timeout must not hide a
    /// tracker from unrelated downloads in the same process.
    pub async fn record_failure_kind(&self, url: &str, kind: TrackerFailureKind) {
        let mut health = self.health.lock().await;
        let state = health.entry(url.to_string()).or_default();
        state.failures = state.failures.saturating_add(1);

        let (consecutive_failures, base_backoff) = match kind {
            TrackerFailureKind::Network | TrackerFailureKind::Timeout => return,
            TrackerFailureKind::RemoteTemporary => (
                state.consecutive_failures.saturating_add(1),
                REMOTE_TEMPORARY_BACKOFF,
            ),
            TrackerFailureKind::MalformedResponse => return,
            TrackerFailureKind::TrackerRejected => return,
        };

        state.consecutive_failures = consecutive_failures;
        let exponent = consecutive_failures.saturating_sub(1).min(10);
        let backoff = base_backoff
            .checked_mul(1u32 << exponent)
            .unwrap_or(MAX_BACKOFF)
            .min(MAX_BACKOFF);
        state.next_retry = Some(Instant::now() + backoff);
    }

    pub async fn last_refresh_error(&self) -> Option<String> {
        self.last_refresh_error.read().await.clone()
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.config_changed.notify_one();
    }

    pub async fn stats(&self) -> PublicTrackerListStats {
        let entries = self.snapshot().await;
        let http_count = entries
            .iter()
            .filter(|e| matches!(e.protocol, TrackerProtocol::Http | TrackerProtocol::Https))
            .count();
        let udp_count = entries
            .iter()
            .filter(|e| e.protocol == TrackerProtocol::Udp)
            .count();
        let wss_count = entries
            .iter()
            .filter(|e| e.protocol == TrackerProtocol::Wss)
            .count();

        let embedded = EMBEDDED_TRACKER_LIST
            .lines()
            .filter(|l| l.trim().contains("://"))
            .count();

        PublicTrackerListStats {
            total_entries: entries.len(),
            http_count,
            udp_count,
            wss_count,
            is_embedded_fallback: entries.len() == embedded,
            last_updated: self.last_updated.read().await.map(|t| t.elapsed()),
            sources: self.sources().await,
            disabled: !self.config.read().await.enabled,
        }
    }
}

async fn fetch_tracker_source(
    fetcher: &dyn TrackerSourceFetcher,
    source: &str,
    request_timeout: Duration,
) -> Result<Vec<TrackerEntry>, String> {
    debug!("Fetching public tracker list from {}", source);
    let body = tokio::time::timeout(request_timeout, fetcher.fetch(source, request_timeout))
        .await
        .map_err(|_| {
            format!(
                "Tracker source request timed out after {} seconds",
                request_timeout.as_secs_f64()
            )
        })??;
    let entries = PublicTrackerList::parse(&body);
    if entries.is_empty() {
        return Err("Parsed 0 entries from response".to_string());
    }
    Ok(entries)
}

struct ReqwestTrackerSourceFetcher {
    client: reqwest::Client,
}

impl ReqwestTrackerSourceFetcher {
    fn new() -> Self {
        ensure_ring_provider();
        let client = reqwest::Client::builder()
            .build()
            .expect("public tracker HTTP client should be constructible");
        Self { client }
    }
}

impl TrackerSourceFetcher for ReqwestTrackerSourceFetcher {
    fn fetch<'a>(
        &'a self,
        source: &'a str,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let response = self
                .client
                .get(source)
                .send()
                .await
                .map_err(|error| format!("HTTP GET failed: {error}"))?;
            if !response.status().is_success() {
                return Err(format!("HTTP error: {}", response.status()));
            }
            response
                .text()
                .await
                .map_err(|error| format!("Read body failed: {error}"))
        })
    }
}

fn normalize_sources(sources: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    sources
        .into_iter()
        .flat_map(|source| {
            source
                .split([',', '\n'])
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|source| !source.is_empty() && seen.insert(source.clone()))
        .collect()
}
