use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::constants;
use crate::engine::command::{PROGRESS_CHANNEL_CAPACITY, ProgressUpdate};
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::http::client_config::{ResolvedNetworkAddresses, build_download_clients};
use crate::engine::http::cookie_helper::CookieHelper;
use crate::error::{Aria2Error, Result};
use crate::http::cookie::{Cookie, CookieStorage};
use crate::http::socks_connector::NoProxyMatcher;
use crate::network::OutboundNetworkPolicy;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::perf_monitor::{AtomicMetrics, Metrics, PerformanceMonitor};
use crate::util::rwlock_ext::RwLockRecover;
use crate::validation::uri::sanitize_filename_from_uri;

use super::DownloadCommand;
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

    /// Create the unified HTTP command from a single-file Metalink document.
    #[cfg(feature = "metalink")]
    pub fn new_from_metalink(
        gid: GroupId,
        metalink_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let document =
            aria2_protocol::metalink::parser::MetalinkDocument::parse(metalink_bytes, None)
                .map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Metalink parse failed: {error}"
                    )))
                })?;
        let file = document.files.first().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Metalink contains no files".into(),
            ))
        })?;
        let urls: Vec<String> = file
            .get_sorted_urls()
            .iter()
            .map(|entry| entry.url.clone())
            .collect();
        let first_url = urls.first().cloned().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "No URLs in Metalink".into(),
            ))
        })?;

        let mut effective_options = options.clone();
        if effective_options.checksum.is_none()
            && let Some(hash) = file.hashes.first()
        {
            effective_options.checksum =
                Some((hash.algo.as_standard_name().to_string(), hash.value.clone()));
        }

        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            urls,
            effective_options.clone(),
        )));
        if let Some(size) = file.size {
            group.recover().set_total_length(size);
        }
        group.recover().set_output_name(file.name.clone());

        Self::new_with_group(
            group,
            &first_url,
            &effective_options,
            output_dir,
            Some(&file.name),
        )
    }

    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
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
        Self::new_with_group_and_resolved_network_addresses(
            group,
            uri,
            options,
            output_dir,
            output_name,
            ResolvedNetworkAddresses {
                target: resolved_addresses,
                proxy: None,
            },
            Arc::new(OutboundNetworkPolicy::direct()),
        )
    }

    pub fn new_with_group_and_resolved_addresses_and_policy(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        resolved_addresses: Option<Vec<std::net::SocketAddr>>,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<Self> {
        Self::new_with_group_and_resolved_network_addresses(
            group,
            uri,
            options,
            output_dir,
            output_name,
            ResolvedNetworkAddresses {
                target: resolved_addresses,
                proxy: None,
            },
            outbound_network_policy,
        )
    }

    pub(crate) fn new_with_group_and_resolved_network_addresses(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        resolved_addresses: ResolvedNetworkAddresses,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<Self> {
        if options.uses_memory_download_for_uri(uri) {
            group.recover().mark_in_memory_download();
        }
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
            .unwrap_or_else(|| sanitize_filename_from_uri(uri));

        let path = std::path::PathBuf::from(&dir).join(&filename);
        group
            .recover()
            .set_resolved_output_path(path.to_string_lossy());
        let request_policy = options.http_request_policy();

        // Every client construction path below must use the same rustls
        // provider. The DNS-cache and proxy branches build custom clients
        // instead of reusing the global pool client.
        crate::http::client_pool::ensure_rustls_provider();
        let client_tls =
            crate::http::client_identity::ClientTlsConfig::from_download_options(options);
        let (client, range_clients) = build_download_clients(
            uri,
            options,
            &client_tls,
            &resolved_addresses,
            &outbound_network_policy,
        )?;

        info!("DownloadCommand created: {} -> {}", uri, path.display());

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = CookieStorage::shared();

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::channel::<ProgressUpdate>(PROGRESS_CHANNEL_CAPACITY);

        Ok(Self {
            group,
            progress,
            client,
            range_clients,
            outbound_network_policy,
            output_path: path,
            output_name_explicit: output_name.is_some(),
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
            .unwrap_or_else(|| sanitize_filename_from_uri(uri));

        let path = std::path::PathBuf::from(&dir).join(&filename);
        group
            .recover()
            .set_resolved_output_path(path.to_string_lossy());

        let request_policy = options.http_request_policy();
        info!(
            "DownloadCommand created (shared client): {} -> {}",
            uri,
            path.display()
        );

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = CookieStorage::shared();

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::channel::<ProgressUpdate>(PROGRESS_CHANNEL_CAPACITY);

        Ok(Self {
            group,
            progress,
            range_clients: Arc::new(vec![client.as_ref().clone()]),
            client,
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            output_path: path,
            output_name_explicit: output_name.is_some(),
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
    pub(crate) fn with_progress_sender(mut self, sender: mpsc::Sender<ProgressUpdate>) -> Self {
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
        if self.group.recover().options().force_sequential {
            return false;
        }
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
