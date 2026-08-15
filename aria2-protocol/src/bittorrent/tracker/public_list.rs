use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::http::client::ensure_ring_provider;

pub const DEFAULT_TRACKER_SOURCE: &str = "https://cf.trackerslist.com/best.txt";
pub const DEFAULT_TRACKER_UPDATE_INTERVAL: Duration = Duration::from_secs(86_400);
const TRACKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(3_600);
const REMOTE_TEMPORARY_BACKOFF: Duration = Duration::from_secs(30);

const EMBEDDED_TRACKER_LIST: &str = "http://1337.abcvg.info:80/announce
http://lucke.fenesisu.moe:6969/announce
http://nyaa.tracker.wf:7777/announce
http://open.acgtracker.com:1096/announce
http://torrentsmd.com:8080/announce
http://tracker.dhitechnical.com:6969/announce
http://tracker.exe.in.th:6969/announce
http://tracker.mywaifu.best:6969/announce
http://tracker.renfei.net:8080/announce
http://tracker.skyts.net:6969/announce
http://tracker.tritan.gg:8080/announce
http://tracker.xn--djrq4gl4hvoi.top:80/announce
http://tracker3.ctix.cn:8080/announce
https://1337.abcvg.info:443/announce
https://cny.fan:443/announce
https://pybittrack.retiolus.net:443/announce
https://shahidrazi.online:443/announce
https://t.213891.xyz:443/announce
https://torrent.tracker.durukanbal.com:443/announce
https://tr.abiir.top:443/announce
https://tr.nyacat.pw:443/announce
https://tracker-zhuqiy.xn--1r3au8b.space:443/announce
https://tracker.7471.top:443/announce
https://tracker.bt4g.com:443/announce
https://tracker.gcrenwp.top:443/announce
https://tracker.ghostchu-services.top:443/announce
https://tracker.kuroy.me:443/announce
https://tracker.manager.v6.navy:443/announce
https://tracker.moeking.me:443/announce
https://tracker.nekomi.cn:443/announce
https://tracker.pmman.tech:443/announce
https://tracker.qingwapt.org:443/announce
https://tracker.yemekyedim.com:443/announce
https://tracker.yggleak.top:443/announce
https://tracker.zhuqiy.com:443/announce
https://tracker1.520.jp:443/announce
udp://admin.52ywp.com:6969/announce
udp://bittorrent-tracker.e-n-c-r-y-p-t.net:1337/announce
udp://bt.rer.lol:6969/announce
udp://evan.im:6969/announce
udp://martin-gebhardt.eu:25/announce
udp://ns575949.ip-51-222-82.net:6969/announce
udp://open.demonii.com:1337/announce
udp://open.stealth.si:80/announce
udp://opentor.org:2710/announce
udp://p4p.arenabg.com:1337/announce
udp://t.overflow.biz:6969/announce
udp://tracker.004430.xyz:1337/announce
udp://tracker.1h.is:1337/announce
udp://tracker.bluefrog.pw:2710/announce
udp://tracker.breizh.pm:6969/announce
udp://tracker.corpscorp.online:80/announce
udp://tracker.darkness.services:6969/announce
udp://tracker.dler.com:6969/announce
udp://tracker.flatuslifir.is:6969/announce
udp://tracker.fnix.net:6969/announce
udp://tracker.gmi.gd:6969/announce
udp://tracker.ixuexi.click:6969/announce
udp://tracker.opentorrent.top:6969/announce
udp://tracker.opentrackr.org:1337/announce
udp://tracker.playground.ru:6969/announce
udp://tracker.plx.im:6969/announce
udp://tracker.qu.ax:6969/announce
udp://tracker.skyts.net:6969/announce
udp://tracker.srv00.com:6969/announce
udp://tracker.t-1.org:6969/announce
udp://tracker.theoks.net:6969/announce
udp://tracker.torrent.eu.org:451/announce
udp://tracker.torrust-demo.com:6969/announce
udp://tracker.tryhackx.org:6969/announce
udp://uabits.today:6990/announce
udp://udp.tracker.projectk.org:23333/announce
udp://wepzone.net:6969/announce
udp://www.nartlof.com:6969/announce
wss://tracker.openwebtorrent.com:443/announce";

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

impl TrackerProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrackerProtocol::Http => "http",
            TrackerProtocol::Https => "https",
            TrackerProtocol::Udp => "udp",
            TrackerProtocol::Wss => "wss",
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

#[derive(Debug, Clone, Default)]
struct TrackerHealth {
    consecutive_failures: u32,
    next_retry: Option<Instant>,
    successes: u64,
    failures: u64,
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

pub struct PublicTrackerList {
    entries: tokio::sync::RwLock<Arc<Vec<TrackerEntry>>>,
    source_entries: tokio::sync::RwLock<HashMap<String, Vec<TrackerEntry>>>,
    http_client: reqwest::Client,
    config: tokio::sync::RwLock<TrackerCatalogConfig>,
    health: Mutex<HashMap<String, TrackerHealth>>,
    last_updated: tokio::sync::RwLock<Option<Instant>>,
    last_refresh_error: tokio::sync::RwLock<Option<String>>,
    running: AtomicBool,
    update_started: AtomicBool,
    config_changed: Notify,
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
        let entries = Self::parse(EMBEDDED_TRACKER_LIST);
        ensure_ring_provider();
        let http_client = reqwest::Client::builder()
            .timeout(TRACKER_REQUEST_TIMEOUT)
            .build()
            .expect("public tracker HTTP client should be constructible");
        Self {
            entries: tokio::sync::RwLock::new(Arc::new(entries)),
            source_entries: tokio::sync::RwLock::new(HashMap::new()),
            http_client,
            config: tokio::sync::RwLock::new(config),
            health: Mutex::new(HashMap::new()),
            last_updated: tokio::sync::RwLock::new(Some(Instant::now())),
            last_refresh_error: tokio::sync::RwLock::new(None),
            running: AtomicBool::new(true),
            update_started: AtomicBool::new(false),
            config_changed: Notify::new(),
        }
    }

    pub fn parse(text: &str) -> Vec<TrackerEntry> {
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || !trimmed.contains("://") {
                continue;
            }

            if let Some(entry) =
                normalize_tracker_url(trimmed).and_then(|url| parse_single_tracker_url(&url))
                && seen.insert(entry.url.clone())
            {
                result.push(entry);
            }
        }
        result
    }

    pub async fn config(&self) -> TrackerCatalogConfig {
        self.config.read().await.clone()
    }

    pub async fn set_config(&self, mut config: TrackerCatalogConfig) {
        config.sources = normalize_sources(config.sources);
        if config.update_interval.is_zero() {
            config.update_interval = DEFAULT_TRACKER_UPDATE_INTERVAL;
        }
        *self.config.write().await = config;
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
        let entries = fetch_tracker_source(&self.http_client, url).await?;
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
            return Err("No public tracker sources configured".to_string());
        }

        let client = self.http_client.clone();
        let results = futures::future::join_all(
            sources
                .iter()
                .map(|source| fetch_tracker_source(&client, source)),
        )
        .await;
        let mut errors = Vec::new();
        let mut source_entries = self.source_entries.write().await;
        source_entries.retain(|source, _| sources.contains(source));
        for (source, result) in sources.iter().zip(results) {
            match result {
                Ok(entries) => {
                    source_entries.insert(source.clone(), entries);
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
    client: &reqwest::Client,
    source: &str,
) -> Result<Vec<TrackerEntry>, String> {
    debug!("Fetching public tracker list from {}", source);
    let response = client
        .get(source)
        .send()
        .await
        .map_err(|e| format!("HTTP GET failed: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("HTTP error: {}", response.status()));
    }
    let body = response
        .text()
        .await
        .map_err(|e| format!("Read body failed: {}", e))?;
    let entries = PublicTrackerList::parse(&body);
    if entries.is_empty() {
        return Err("Parsed 0 entries from response".to_string());
    }
    Ok(entries)
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

fn normalize_tracker_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "udp" | "wss") {
        return None;
    }
    if parsed.host_str()?.is_empty() {
        return None;
    }
    if !parsed.path().is_empty() && parsed.path() != "/announce" && parsed.path() != "/announce/" {
        return None;
    }
    let without_fragment = raw.split('#').next().unwrap_or(raw);
    if parsed.path().is_empty() {
        Some(format!("{without_fragment}/announce"))
    } else if parsed.path() == "/announce/" {
        Some(without_fragment.trim_end_matches('/').to_string())
    } else {
        Some(without_fragment.to_string())
    }
}

fn parse_single_tracker_url(url: &str) -> Option<TrackerEntry> {
    let parsed = url::Url::parse(url).ok()?;
    let protocol = if parsed.scheme() == "https" {
        TrackerProtocol::Https
    } else if parsed.scheme() == "http" {
        TrackerProtocol::Http
    } else if parsed.scheme() == "udp" {
        TrackerProtocol::Udp
    } else if parsed.scheme() == "wss" {
        TrackerProtocol::Wss
    } else {
        return None;
    };

    let default_port = match protocol {
        TrackerProtocol::Http => 80,
        TrackerProtocol::Https | TrackerProtocol::Wss => 443,
        TrackerProtocol::Udp => 6969,
    };
    let host = parsed.host_str()?.to_string();
    let port = parsed.port().unwrap_or(default_port);
    if host.is_empty() || port == 0 || parsed.path() != "/announce" {
        return None;
    }

    Some(TrackerEntry {
        url: url.to_string(),
        protocol,
        host,
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_embedded_list() {
        let entries = PublicTrackerList::parse(EMBEDDED_TRACKER_LIST);
        assert!(
            entries.len() >= 50,
            "embedded list should have at least 50 trackers, got {}",
            entries.len()
        );
    }

    #[test]
    fn test_parse_empty_input() {
        let entries = PublicTrackerList::parse("");
        assert!(entries.is_empty(), "empty input should produce empty list");
    }

    #[test]
    fn test_parse_whitespace_only() {
        let entries = PublicTrackerList::parse("\n\n   \n\t\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_mixed_protocols() {
        let text = "http://a.com:80/announce\nhttps://b.com:443/announce\nudp://c.com:6969/announce\nwss://d.com:443/announce";
        let entries = PublicTrackerList::parse(text);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].protocol, TrackerProtocol::Http);
        assert_eq!(entries[1].protocol, TrackerProtocol::Https);
        assert_eq!(entries[2].protocol, TrackerProtocol::Udp);
        assert_eq!(entries[3].protocol, TrackerProtocol::Wss);
    }

    #[test]
    fn test_parse_invalid_lines_skipped() {
        let text = "http://valid.com:80/announce\nnot a url\n\nhttp://another.com/announce";
        let entries = PublicTrackerList::parse(text);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_get_http_trackers_filters_correctly() {
        let ptl = PublicTrackerList::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let http = ptl.get_http_trackers().await;
            for url in &http {
                assert!(
                    url.starts_with("http://") || url.starts_with("https://"),
                    "{} should be http or https",
                    url
                );
                assert!(!url.starts_with("udp://"), "{} should not be udp", url);
            }
            assert!(!http.is_empty(), "should have at least some HTTP trackers");
        });
    }

    #[test]
    fn test_get_udp_trackers_filters_correctly() {
        let ptl = PublicTrackerList::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let udp = ptl.get_udp_trackers().await;
            for url in &udp {
                assert!(url.starts_with("udp://"), "{} should be udp", url);
            }
            assert!(!udp.is_empty(), "should have at least some UDP trackers");
        });
    }

    #[tokio::test]
    async fn test_stats_returns_reasonable_values() {
        let ptl = PublicTrackerList::new();
        let stats = ptl.stats().await;
        assert!(stats.total_entries >= 50);
        assert!(stats.http_count > 0);
        assert!(stats.udp_count > 0);
        assert!(
            stats.is_embedded_fallback,
            "default instance uses embedded list"
        );
        assert!(stats.last_updated.is_some());
    }

    #[tokio::test]
    async fn test_default_instance_uses_embedded() {
        let ptl = PublicTrackerList::new();
        let all = ptl.get_all().await;
        let embedded_count = PublicTrackerList::parse(EMBEDDED_TRACKER_LIST).len();
        assert_eq!(
            all.len(),
            embedded_count,
            "default instance should use all embedded entries"
        );
    }

    #[tokio::test]
    async fn disabled_catalog_has_no_available_trackers() {
        let ptl = PublicTrackerList::new();
        ptl.set_config(TrackerCatalogConfig {
            enabled: false,
            sources: Vec::new(),
            update_interval: Duration::from_secs(60),
        })
        .await;

        assert!(ptl.available_snapshot().await.is_empty());
        assert!(ptl.get_http_trackers().await.is_empty());
        assert!(ptl.get_udp_trackers().await.is_empty());
        assert!(
            !ptl.get_all().await.is_empty(),
            "disabling availability must not erase the catalog snapshot"
        );
    }

    #[tokio::test]
    async fn reenabled_catalog_exposes_existing_snapshot() {
        let ptl = PublicTrackerList::new();
        ptl.set_config(TrackerCatalogConfig {
            enabled: false,
            sources: Vec::new(),
            update_interval: Duration::from_secs(60),
        })
        .await;
        ptl.set_config(TrackerCatalogConfig {
            enabled: true,
            sources: Vec::new(),
            update_interval: Duration::from_secs(60),
        })
        .await;

        assert!(!ptl.available_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn tracker_backoff_preserves_catalog_and_success_recovers() {
        let ptl = PublicTrackerList::new();
        let entry = PublicTrackerList::parse("http://unhealthy.example/announce")
            .into_iter()
            .next()
            .expect("test tracker should parse");
        ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

        ptl.record_failure(&entry.url).await;
        ptl.record_failure(&entry.url).await;
        assert!(ptl.contains(&entry.url).await);
        ptl.record_failure(&entry.url).await;

        assert!(ptl.contains(&entry.url).await);
        assert!(ptl.available_snapshot().await.is_empty());

        ptl.record_success(&entry.url).await;
        assert!(!ptl.available_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn local_failures_do_not_cool_or_remove_tracker() {
        let ptl = PublicTrackerList::new();
        let entry = PublicTrackerList::parse("http://local-network.example/announce")
            .into_iter()
            .next()
            .expect("test tracker should parse");
        ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

        ptl.record_failure_kind(&entry.url, TrackerFailureKind::Network)
            .await;
        ptl.record_failure_kind(&entry.url, TrackerFailureKind::Timeout)
            .await;

        assert!(ptl.contains(&entry.url).await);
        assert!(
            ptl.available_snapshot()
                .await
                .iter()
                .any(|candidate| candidate.url == entry.url)
        );
    }

    #[tokio::test]
    async fn remote_temporary_failure_is_retriable_and_preserves_catalog() {
        let ptl = PublicTrackerList::new();
        let entry = PublicTrackerList::parse("http://temporary.example/announce")
            .into_iter()
            .next()
            .expect("test tracker should parse");
        ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

        ptl.record_failure_kind(&entry.url, TrackerFailureKind::RemoteTemporary)
            .await;

        assert!(ptl.contains(&entry.url).await);
        assert!(ptl.available_snapshot().await.is_empty());
        ptl.record_success(&entry.url).await;
        assert!(!ptl.available_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn explicit_tracker_failure_does_not_hide_or_remove_tracker() {
        let ptl = PublicTrackerList::new();
        let entry = PublicTrackerList::parse("http://rejected.example/announce")
            .into_iter()
            .next()
            .expect("test tracker should parse");
        ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

        ptl.record_failure_kind(&entry.url, TrackerFailureKind::TrackerRejected)
            .await;

        assert!(ptl.contains(&entry.url).await);
        assert!(
            ptl.available_snapshot()
                .await
                .iter()
                .any(|candidate| candidate.url == entry.url)
        );
    }

    #[test]
    fn test_parse_non_announce_path_rejected() {
        let text = "http://valid.com:80/scrape\nhttp://valid.com:80/announce";
        let entries = PublicTrackerList::parse(text);
        assert_eq!(entries.len(), 1, "non-announce paths should be rejected");
        assert_eq!(entries[0].url, "http://valid.com:80/announce");
    }

    #[tokio::test]
    async fn test_shutdown_sets_flag() {
        let ptl = PublicTrackerList::new();
        assert!(ptl.running.load(Ordering::Relaxed));
        ptl.shutdown();
        assert!(!ptl.running.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_start_auto_update_starts_task() {
        use std::time::Duration as StdDuration;
        let ptl = Arc::new(PublicTrackerList::new());
        ptl.start_auto_update(
            "https://example.com/fake.txt".to_string(),
            StdDuration::from_secs(999999),
        );
        sleep(Duration::from_millis(50)).await;
        assert!(ptl.running.load(Ordering::Relaxed));
        ptl.shutdown();
    }
}
