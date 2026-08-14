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
use crate::http::HttpRequestPolicy;
use crate::http::cookie::Cookie;
use crate::http::cookie_storage::CookieStorage;
use crate::http::socks_connector::{NoProxyMatcher, ProxyUrl};
use crate::rate_limiter::RateLimiter;
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
    /// URI selected when this command was created.
    ///
    /// A request group may contain mirror URIs. Keeping the command's
    /// selected URI separate from the group's snapshot prevents a later
    /// runtime URI update from silently changing the request that is already
    /// being attempted.
    pub(super) initial_uri: String,
    pub(super) output_path: std::path::PathBuf,
    /// Whether the output path has already gone through collision resolution.
    /// Mirror failover reuses this resolved path after the prior attempt
    /// releases its temporary registry claim.
    pub(super) output_path_resolved: bool,
    pub(super) started: bool,
    pub(super) completed: bool,
    pub(super) completed_bytes: u64,
    pub(super) file_allocation: String,
    pub(super) mmap_threshold: u64,
    pub(super) secure_falloc: bool,
    /// `--check-integrity`: verify existing data against context piece hashes
    /// before downloading (C++ `CheckIntegrityMan`). Only meaningful when the
    /// DownloadContext carries piece hashes (e.g. Metalink).
    pub(super) check_integrity: bool,
    pub(super) cookie_storage: Arc<CookieStorage>,
    pub(super) cookie_file: Option<String>,
    pub(super) no_proxy_matcher: Option<NoProxyMatcher>,
    pub(super) stat_man: Arc<ServerStatMan>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` / segment download loops
    /// so that all concurrent downloads share a single bandwidth ceiling.
    pub(super) global_limiter: Option<RateLimiter>,
    pub(super) perf_monitor: Option<Arc<PerformanceMonitor>>,
    pub(super) atomic_metrics: Arc<AtomicMetrics>,
    pub(super) request_policy: HttpRequestPolicy,
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

fn uri_host(uri: &str) -> Option<String> {
    reqwest::Url::parse(uri).ok()?.host_str().map(str::to_owned)
}

#[derive(Clone, Copy, Debug)]
enum ProxyTarget {
    Http,
    Https,
    All,
}

fn build_reqwest_proxy(
    target: ProxyTarget,
    proxy_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    no_proxy: Option<&str>,
) -> std::result::Result<reqwest::Proxy, reqwest::Error> {
    let mut proxy = match target {
        ProxyTarget::Http => reqwest::Proxy::http(proxy_url)?,
        ProxyTarget::Https => reqwest::Proxy::https(proxy_url)?,
        ProxyTarget::All => reqwest::Proxy::all(proxy_url)?,
    };

    // Preserve credentials embedded in the proxy URL unless an option
    // explicitly overrides them, matching AbstractCommand::makeProxyUri().
    if username.is_some() || password.is_some() {
        let embedded = proxy_url.parse::<reqwest::Url>().ok();
        let embedded_user = embedded
            .as_ref()
            .filter(|url| !url.username().is_empty())
            .map(|url| url.username().to_string());
        let embedded_password = embedded
            .as_ref()
            .and_then(|url| url.password().map(str::to_string));
        let effective_user = username.map(str::to_owned).or(embedded_user);
        let effective_password = password
            .map(str::to_owned)
            .or(embedded_password)
            .unwrap_or_default();

        if let Some(user) = effective_user {
            proxy = proxy.basic_auth(&user, &effective_password);
        }
    }

    if let Some(no_proxy) = no_proxy {
        proxy = proxy.no_proxy(reqwest::NoProxy::from_string(no_proxy));
    }

    Ok(proxy)
}

fn add_reqwest_proxy(
    builder: reqwest::ClientBuilder,
    target: ProxyTarget,
    proxy_url: &str,
    credentials: (Option<String>, Option<String>),
    no_proxy: Option<&str>,
) -> reqwest::ClientBuilder {
    match build_reqwest_proxy(
        target,
        proxy_url,
        credentials.0.as_deref(),
        credentials.1.as_deref(),
        no_proxy,
    ) {
        Ok(proxy) => builder.proxy(proxy),
        Err(error) => {
            warn!(%proxy_url, ?target, %error, "Ignoring invalid HTTP proxy configuration");
            builder
        }
    }
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
        if options.uses_memory_download() {
            group.recover().mark_in_memory_download();
        }
        Self::new_with_group(group, uri, options, output_dir, output_name)
    }

    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        if options.uses_memory_download() {
            group.recover().mark_in_memory_download();
        }
        Self::new_with_group_and_resolved_addresses(
            group,
            uri,
            options,
            output_dir,
            output_name,
            None,
        )
    }

    pub fn new_with_group_and_resolved_addresses(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        resolved_addresses: Option<Vec<std::net::SocketAddr>>,
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
        let request_policy = options.http_request_policy();

        // Every client construction path below must use the same rustls
        // provider. The DNS-cache and proxy branches build custom clients
        // instead of reusing the global pool client.
        crate::http::client_pool::ensure_rustls_provider();
        let client_tls =
            crate::http::client_identity::ClientTlsConfig::from_download_options(options);
        let no_proxy = ![
            options.http_proxy.as_deref(),
            options.https_proxy.as_deref(),
            options.all_proxy.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|proxy| !proxy.is_empty());
        let has_custom_tls = client_tls.requires_custom_client();
        let client = if no_proxy {
            if let Some(addresses) = resolved_addresses.as_deref()
                && !addresses.is_empty()
            {
                let host = uri_host(uri).ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "Unable to extract HTTP hostname for DNS cache override".to_string(),
                    ))
                })?;
                let builder = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(
                        constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
                    ))
                    .timeout(Duration::from_secs(
                        constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                    ))
                    .gzip(options.http_accept_gzip)
                    .user_agent(constants::USER_AGENT)
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve_to_addrs(&host, addresses);
                let builder = crate::http::client_identity::apply(builder, &client_tls)?;
                Arc::new(builder.build().map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Failed to build HTTP client with DNS cache: {e}"
                    )))
                })?)
            } else if options.http_accept_gzip || has_custom_tls {
                let builder = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(
                        constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
                    ))
                    .timeout(Duration::from_secs(
                        constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                    ))
                    .gzip(options.http_accept_gzip)
                    .user_agent(constants::USER_AGENT)
                    .redirect(reqwest::redirect::Policy::none())
                    .pool_max_idle_per_host(constants::HTTP_CLIENT_POOL_MAX_IDLE_PER_HOST)
                    .pool_idle_timeout(Some(Duration::from_secs(
                        constants::HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
                    )))
                    .tcp_keepalive(Some(Duration::from_secs(
                        constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
                    )));
                let builder = crate::http::client_identity::apply(builder, &client_tls)?;
                Arc::new(builder.build().map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Failed to build HTTP client: {error}"
                    )))
                })?)
            } else {
                crate::http::client_pool::get_global_client()
            }
        } else {
            let mut builder = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
                ))
                .timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                ))
                .gzip(options.http_accept_gzip)
                .user_agent(constants::USER_AGENT)
                // Redirects are handled by SequentialDownloader so direct,
                // DNS-pinned, and proxied clients share one URI/retry seam.
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(constants::HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST)
                .pool_idle_timeout(Some(std::time::Duration::from_secs(
                    constants::HTTP_DEFAULT_POOL_IDLE_TIMEOUT_SECS,
                )))
                .tcp_keepalive(Some(std::time::Duration::from_secs(
                    constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
                )));

            let no_proxy = options.no_proxy.as_deref();

            if let Some(proxy) = options
                .http_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
            {
                builder = add_reqwest_proxy(
                    builder,
                    ProxyTarget::Http,
                    proxy,
                    options.proxy_credentials_for_scheme("http"),
                    no_proxy,
                );
            }

            if let Some(proxy) = options
                .https_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
            {
                builder = add_reqwest_proxy(
                    builder,
                    ProxyTarget::Https,
                    proxy,
                    options.proxy_credentials_for_scheme("https"),
                    no_proxy,
                );
            }

            if let Some(all_proxy) = options
                .all_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
            {
                match ProxyUrl::parse(all_proxy) {
                    Ok(parsed) => match parsed.protocol {
                        crate::http::socks_connector::ProxyProtocol::Http
                        | crate::http::socks_connector::ProxyProtocol::Https => {
                            builder = add_reqwest_proxy(
                                builder,
                                ProxyTarget::All,
                                all_proxy,
                                options.proxy_credentials_for_scheme("all"),
                                no_proxy,
                            );
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

            let builder = crate::http::client_identity::apply(builder, &client_tls)?;
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
        let cookie_storage = CookieStorage::shared();

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        Ok(Self {
            group,
            progress,
            client,
            initial_uri: uri.to_string(),
            output_path: path,
            output_path_resolved: false,
            started: false,
            completed: false,
            completed_bytes: 0,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| constants::DEFAULT_FILE_ALLOCATION.to_string()),
            mmap_threshold: options.mmap_threshold.unwrap_or(256 * 1024 * 1024),
            secure_falloc: options.secure_falloc,
            check_integrity: options.check_integrity,
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: ServerStatMan::shared().clone(),
            global_limiter: None,
            perf_monitor: None,
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            request_policy,
            progress_sender: Some(progress_tx),
            progress_receiver: Some(progress_rx),
            progress_aggregator_handle: None,
            // Tail reclaim fields — mirrors C++ DownloadCommand constructor.
            last_tail_reclaim_session_download_length: 0,
            tail_reclaim_last_progress: Instant::now(),
            startup_idle_time: Duration::from_secs(options.startup_idle_time.unwrap_or(10)),
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

        let request_policy = options.http_request_policy();
        info!(
            "DownloadCommand created (shared client): {} -> {}",
            uri,
            path.display()
        );

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = CookieStorage::shared();

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        Ok(Self {
            group,
            progress,
            client,
            initial_uri: uri.to_string(),
            output_path: path,
            output_path_resolved: false,
            started: false,
            completed: false,
            completed_bytes: 0,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| constants::DEFAULT_FILE_ALLOCATION.to_string()),
            mmap_threshold: options.mmap_threshold.unwrap_or(256 * 1024 * 1024),
            secure_falloc: options.secure_falloc,
            check_integrity: options.check_integrity,
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: ServerStatMan::shared().clone(),
            global_limiter: None,
            perf_monitor: None,
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            request_policy,
            progress_sender: Some(progress_tx),
            progress_receiver: Some(progress_rx),
            progress_aggregator_handle: None,
            // Tail reclaim fields — mirrors C++ DownloadCommand constructor.
            last_tail_reclaim_session_download_length: 0,
            tail_reclaim_last_progress: Instant::now(),
            startup_idle_time: Duration::from_secs(options.startup_idle_time.unwrap_or(10)),
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

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, the download paths (sequential and concurrent) will acquire
    /// tokens from this limiter in addition to the per-download limiter,
    /// enforcing a global bandwidth ceiling across all concurrent downloads.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
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
            self.group.recover().global_net_stat(),
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
            Ok(g) if g.is_force_halt_requested() => {
                Err(Aria2Error::DownloadFailed("Download halted".into()))
            }
            Ok(g) if g.is_halt_requested() => {
                Err(Aria2Error::DownloadFailed("Download halted".into()))
            }
            _ => Ok(()),
        }
    }
}
