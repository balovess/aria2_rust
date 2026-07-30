mod execute;
mod tail_reclaim;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::error::{Aria2Error, Result};
use crate::http::cookie::Cookie;
use crate::http::cookie_storage::CookieStorage;
use crate::http::socks_connector::{NoProxyMatcher, ProxyUrl};
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::perf_monitor::{AtomicMetrics, Metrics, PerformanceMonitor};
use crate::util::rwlock_ext::RwLockRecover;

/// Core download command that handles HTTP/HTTPS file downloads.
///
/// Supports both sequential and concurrent (range-based) download strategies,
/// with automatic resume, cookie management, proxy configuration, and
/// checksum verification.
pub struct DownloadCommand {
    pub(super) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters -- avoids RwLock on the hot path.
    pub(super) progress: Arc<AtomicProgress>,
    pub(super) client: Arc<reqwest::Client>,
    pub(super) output_path: std::path::PathBuf,
    pub(super) started: bool,
    pub(super) completed: bool,
    pub(super) completed_bytes: u64,
    pub(super) file_allocation: String,
    pub(super) mmap_threshold: u64,
    pub(super) secure_falloc: bool,
    pub(super) cookie_storage: Arc<CookieStorage>,
    pub(super) cookie_file: Option<String>,
    pub(super) no_proxy_matcher: Option<NoProxyMatcher>,
    pub(super) stat_man: Arc<ServerStatMan>,
    pub(super) perf_monitor: Option<Arc<PerformanceMonitor>>,
    pub(super) atomic_metrics: Arc<AtomicMetrics>,
    pub(super) headers: Vec<(String, String)>,
    pub(super) progress_sender: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    pub(super) progress_receiver: Option<mpsc::UnboundedReceiver<ProgressUpdate>>,
    pub(super) progress_aggregator_handle: Option<tokio::task::JoinHandle<()>>,

    // ── Tail reclaim progress tracking ─────────────────────────────────
    // Mirrors C++ DownloadCommand fields:
    //   lastTailReclaimSessionDownloadLength_, tailReclaimLastProgress_,
    //   startupIdleTime_, lowestDownloadSpeedLimit_
    //
    // These fields track when data was last received so that the tail
    // reclaim policy can detect stalled connections.  In C++ these are
    // updated on every data chunk via updateTailReclaimProgress().  In Rust
    // they are updated via update_tail_reclaim_progress() which reads from
    // the lock-free AtomicProgress counter.

    /// Completed length at the last time progress was detected.
    /// Mirrors C++ `lastTailReclaimSessionDownloadLength_`.
    pub(super) last_tail_reclaim_session_download_length: u64,

    /// Timestamp of the last time progress was detected.
    /// Mirrors C++ `tailReclaimLastProgress_`.
    pub(super) tail_reclaim_last_progress: Instant,

    /// Stall threshold — if no progress for this duration, the connection
    /// is considered stalled.  Mirrors C++ `startupIdleTime_`.
    /// Defaults to 10 seconds (C++ `PREF_STARTUP_IDLE_TIME` default).
    pub(super) startup_idle_time: Duration,

    /// Lowest download speed limit in bytes/sec.  Downloads slower than
    /// this are aborted.  Mirrors C++ `lowestDownloadSpeedLimit_`.
    /// 0 means no limit.
    pub(super) lowest_speed_limit: u64,
}

impl DownloadCommand {
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group(group, uri, options, output_dir, output_name)
    }

    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let progress = group
            .try_read()
            .map(|g| g.progress.clone())
            .unwrap_or_else(|_| Arc::new(AtomicProgress::new()));
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(uri))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);
        let headers = options.parsed_headers();

        let no_proxy = options.http_proxy.is_none() && options.all_proxy.is_none();
        let client = if no_proxy {
            crate::http::client_pool::get_global_client()
        } else {
            let mut builder = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
                ))
                .timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                ))
                .user_agent(constants::USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(
                    constants::HTTP_DEFAULT_MAX_REDIRECTS,
                ))
                .pool_max_idle_per_host(constants::HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST)
                .pool_idle_timeout(Some(std::time::Duration::from_secs(
                    constants::HTTP_DEFAULT_POOL_IDLE_TIMEOUT_SECS,
                )))
                .tcp_keepalive(Some(std::time::Duration::from_secs(
                    constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
                )));

            if let Some(ref proxy) = options.http_proxy
                && let Ok(proxy_url) = proxy.parse::<reqwest::Url>()
                && let Ok(p) = reqwest::Proxy::all(proxy_url.to_string())
            {
                builder = builder.proxy(p);
            }

            if options.http_proxy.is_none()
                && let Some(ref all_proxy) = options.all_proxy
            {
                match ProxyUrl::parse(all_proxy) {
                    Ok(parsed) => match parsed.protocol {
                        crate::http::socks_connector::ProxyProtocol::Http
                        | crate::http::socks_connector::ProxyProtocol::Https => {
                            if let Ok(p) = reqwest::Proxy::all(all_proxy.to_string()) {
                                builder = builder.proxy(p);
                            }
                        }
                        _ => {
                            tracing::info!(
                                "SOCKS proxy configured ({}) - use SocksConnector for direct TCP connections",
                                all_proxy
                            );
                        }
                    },
                    Err(e) => {
                        warn!("Failed to parse all-proxy URL '{}': {}", all_proxy, e);
                    }
                }
            }

            let client = builder.build().map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Failed to build HTTP client: {}",
                    e
                )))
            })?;

            Arc::new(client)
        };

        info!("DownloadCommand created: {} -> {}", uri, path.display());

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = Arc::new(CookieStorage::new());

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        Ok(Self {
            group,
            progress,
            client,
            output_path: path,
            started: false,
            completed: false,
            completed_bytes: 0,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| constants::DEFAULT_FILE_ALLOCATION.to_string()),
            mmap_threshold: options.mmap_threshold.unwrap_or(256 * 1024 * 1024),
            secure_falloc: options.secure_falloc,
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: Arc::new(ServerStatMan::new()),
            perf_monitor: None,
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            headers,
            progress_sender: Some(progress_tx),
            progress_receiver: Some(progress_rx),
            progress_aggregator_handle: None,
            // Tail reclaim fields — mirrors C++ DownloadCommand constructor.
            last_tail_reclaim_session_download_length: 0,
            tail_reclaim_last_progress: Instant::now(),
            startup_idle_time: Duration::from_secs(
                options.startup_idle_time.unwrap_or(10),
            ),
            lowest_speed_limit: options.lowest_speed_limit.unwrap_or(0),
        })
    }

    fn load_cookies(
        cookie_storage: &Arc<CookieStorage>,
        cookie_file: &Option<String>,
        uri: &str,
        options: &DownloadOptions,
    ) {
        if let Some(cf) = cookie_file {
            let p = std::path::Path::new(cf);
            if p.exists() {
                match cookie_storage.load_file(p) {
                    Ok(n) => info!("Loaded {} cookies from file: {}", n, cf),
                    Err(e) => warn!("Failed to load cookie file {}: {}", cf, e),
                }
            }
        }

        if let Some(ref cookies_str) = options.cookies {
            let domain = Self::extract_host(uri);
            for pair in cookies_str.split(';') {
                let pair = pair.trim();
                if pair.is_empty() {
                    continue;
                }
                if let Some((name, value)) = pair.split_once('=') {
                    let name = name.trim();
                    let value = value.trim();
                    if !name.is_empty() {
                        cookie_storage.add(Cookie::new(name, value, &domain));
                    }
                }
            }
            if !cookie_storage.is_empty() {
                info!("Manually set {} cookies", cookie_storage.count());
            }
        }
    }

    pub fn new_with_client(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        client: Arc<reqwest::Client>,
    ) -> Result<Self> {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group_and_client(group, uri, options, output_dir, output_name, client)
    }

    pub fn new_with_group_and_client(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        client: Arc<reqwest::Client>,
    ) -> Result<Self> {
        let progress = group
            .try_read()
            .map(|g| g.progress.clone())
            .unwrap_or_else(|_| Arc::new(AtomicProgress::new()));
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(uri))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        let headers = options.parsed_headers();
        info!(
            "DownloadCommand created (shared client): {} -> {}",
            uri,
            path.display()
        );

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = Arc::new(CookieStorage::new());

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        Ok(Self {
            group,
            progress,
            client,
            output_path: path,
            started: false,
            completed: false,
            completed_bytes: 0,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| constants::DEFAULT_FILE_ALLOCATION.to_string()),
            mmap_threshold: options.mmap_threshold.unwrap_or(256 * 1024 * 1024),
            secure_falloc: options.secure_falloc,
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: Arc::new(ServerStatMan::new()),
            perf_monitor: None,
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            headers,
            progress_sender: Some(progress_tx),
            progress_receiver: Some(progress_rx),
            progress_aggregator_handle: None,
            // Tail reclaim fields — mirrors C++ DownloadCommand constructor.
            last_tail_reclaim_session_download_length: 0,
            tail_reclaim_last_progress: Instant::now(),
            startup_idle_time: Duration::from_secs(
                options.startup_idle_time.unwrap_or(10),
            ),
            lowest_speed_limit: options.lowest_speed_limit.unwrap_or(0),
        })
    }

    pub fn new_with_stat_man(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        stat_man: Arc<ServerStatMan>,
    ) -> Result<Self> {
        let mut cmd = Self::new(gid, uri, options, output_dir, output_name)?;
        cmd.stat_man = stat_man;
        Ok(cmd)
    }

    pub fn enable_perf_monitor(&mut self) {
        self.perf_monitor = Some(Arc::new(PerformanceMonitor::new()));
    }

    #[allow(dead_code)]
    pub(crate) fn with_progress_sender(
        mut self,
        sender: mpsc::UnboundedSender<ProgressUpdate>,
    ) -> Self {
        self.progress_sender = Some(sender);
        self.progress_receiver = None;
        self
    }

    pub(crate) fn spawn_progress_aggregator(&mut self) {
        if self.progress_aggregator_handle.is_some() {
            return;
        }
        if let Some(rx) = self.progress_receiver.take() {
            let handle = crate::engine::download_engine::DownloadEngine::spawn_progress_aggregator(
                Arc::clone(&self.group),
                Arc::clone(&self.progress),
                rx,
            );
            self.progress_aggregator_handle = Some(handle);
        }
    }

    pub(crate) async fn drain_progress_aggregator(&mut self) {
        self.progress_sender = None;
        if let Some(handle) = self.progress_aggregator_handle.take()
            && let Err(e) = handle.await
        {
            warn!("Progress aggregator task ended unexpectedly: {}", e);
        }
    }

    pub fn get_perf_metrics(&self) -> Metrics {
        self.atomic_metrics.snapshot()
    }

    pub fn get_perf_report(&self) -> Option<String> {
        self.perf_monitor.as_ref().map(|m| m.export_text())
    }

    pub fn get_perf_report_json(&self) -> Option<String> {
        self.perf_monitor.as_ref().map(|m| m.export_json())
    }

    fn extract_filename(uri: &str) -> Option<String> {
        uri.rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "/")
            .map(|s| s.split('?').next().unwrap_or(s).to_string())
    }

    fn extract_host(uri: &str) -> String {
        reqwest::Url::parse(uri)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_else(|| constants::DEFAULT_HOST.to_string())
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    pub fn group_mut(&self) -> std::sync::RwLockWriteGuard<'_, RequestGroup> {
        self.group.recover_mut()
    }

    pub fn no_proxy_matcher(&self) -> Option<&NoProxyMatcher> {
        self.no_proxy_matcher.as_ref()
    }

    pub(super) fn should_use_concurrent(
        &self,
        total_length: u64,
        supports_range: bool,
        split: u16,
    ) -> bool {
        if !supports_range {
            return false;
        }
        if total_length < constants::CONCURRENT_MIN_FILE_SIZE as u64 {
            return false;
        }
        split > 1
    }

    pub(super) fn create_cookie_helper(&self) -> CookieHelper {
        CookieHelper::new(Arc::clone(&self.cookie_storage), self.cookie_file.clone())
    }

    pub(super) fn create_progress_updater(&self) -> ProgressUpdater {
        ProgressUpdater::new(
            self.progress_sender.clone(),
            Arc::clone(&self.group),
            Arc::clone(&self.progress),
            Arc::clone(&self.atomic_metrics),
            self.perf_monitor.clone(),
        )
    }

    /// Non-blocking check whether the underlying RequestGroup has been
    /// cancelled (status set to Removed by aria2.remove /
    /// aria2.forceRemove) or paused (status set to Paused by
    /// aria2.pause / aria2.forcePause).
    ///
    /// Returns Err with a DownloadFailed error when the group has been
    /// removed or paused, so the caller can abort the download promptly.
    /// Uses try_read on the outer group lock so it is safe to call from
    /// hot download loops; when the lock is contended the method treats the
    /// download as still running (returns Ok(())) and the caller will
    /// re-check on the next iteration.
    pub(super) fn check_cancelled(&self) -> Result<()> {
        match self.group.try_read() {
            Ok(g) if g.is_removed() => Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            )),
            Ok(g) if g.is_paused_flag() => {
                Err(Aria2Error::DownloadFailed("Download paused".into()))
            }
            _ => Ok(()),
        }
    }
}
