use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::HashType;
use crate::constants;
use crate::engine::active_output_registry::global_registry;
use crate::engine::command::{Command, CommandStatus, ProgressUpdate};
use crate::engine::concurrent_download::{ConcurrentDownloadResult, ConcurrentDownloader};
use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::range_prober::RangeProber;
use crate::engine::retry_policy::RetryPolicy;
use crate::engine::sequential_download::SequentialDownloader;
use crate::error::{Aria2Error, Result};
use crate::filesystem::file_allocation;
use crate::filesystem::resume_helper::ResumeHelper;
use crate::http::client_pool;
use crate::http::cookie::Cookie;
use crate::http::cookie_storage::CookieStorage;
use crate::http::socks_connector::{NoProxyMatcher, ProxyUrl};
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::perf_monitor::{AtomicMetrics, Metrics, PerformanceMonitor};
use crate::util::rwlock_ext::RwLockRecover;

pub struct DownloadCommand {
    group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters — avoids `RwLock` on the hot path.
    progress: Arc<AtomicProgress>,
    client: Arc<reqwest::Client>,
    output_path: std::path::PathBuf,
    started: bool,
    completed: bool,
    completed_bytes: u64,
    file_allocation: String,
    mmap_threshold: u64,
    secure_falloc: bool,
    cookie_storage: Arc<CookieStorage>,
    cookie_file: Option<String>,
    no_proxy_matcher: Option<NoProxyMatcher>,
    stat_man: Arc<ServerStatMan>,
    perf_monitor: Option<Arc<PerformanceMonitor>>,
    atomic_metrics: Arc<AtomicMetrics>,
    headers: Vec<(String, String)>,
    progress_sender: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    progress_receiver: Option<mpsc::UnboundedReceiver<ProgressUpdate>>,
    progress_aggregator_handle: Option<tokio::task::JoinHandle<()>>,
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
            client_pool::get_global_client()
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

    fn should_use_concurrent(&self, total_length: u64, supports_range: bool, split: u16) -> bool {
        if !supports_range {
            return false;
        }
        if total_length < constants::CONCURRENT_MIN_FILE_SIZE as u64 {
            return false;
        }
        split > 1
    }

    fn create_cookie_helper(&self) -> CookieHelper {
        CookieHelper::new(Arc::clone(&self.cookie_storage), self.cookie_file.clone())
    }

    fn create_progress_updater(&self) -> ProgressUpdater {
        ProgressUpdater::new(
            self.progress_sender.clone(),
            Arc::clone(&self.group),
            Arc::clone(&self.progress),
            Arc::clone(&self.atomic_metrics),
            self.perf_monitor.clone(),
        )
    }

    /// Non-blocking check whether the underlying RequestGroup has been
    /// cancelled (status set to `Removed` by `aria2.remove` /
    /// `aria2.forceRemove`) or paused (status set to `Paused` by
    /// `aria2.pause` / `aria2.forcePause`).
    ///
    /// Returns `Err` with a `DownloadFailed` error when the group has been
    /// removed or paused, so the caller can abort the download promptly.
    /// Uses `try_read` on the outer group lock so it is safe to call from
    /// hot download loops; when the lock is contended the method treats the
    /// download as still running (returns `Ok(())`) and the caller will
    /// re-check on the next iteration.
    fn check_cancelled(&self) -> Result<()> {
        match self.group.try_read() {
            Ok(g) if g.is_removed() => Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            )),
            Ok(g) if g.is_paused_flag() => Err(Aria2Error::DownloadFailed(
                "Download paused".into(),
            )),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl Command for DownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        // Check for early cancellation (task removed before execution started).
        self.check_cancelled()?;

        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        let uri = {
            let g = self.group.recover();
            g.uris().first().cloned().unwrap_or_default()
        };

        if uri.is_empty() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "Download URI is empty".into(),
            )));
        }

        debug!(
            "Starting download: {} -> {}",
            uri,
            self.output_path.display()
        );

        // Re-check cancellation after the HEAD probe / pre-allocation work so
        // a remove issued while we were doing filesystem setup is honoured
        // before any network transfer begins.
        self.check_cancelled()?;

        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Failed to create directory: {}",
                    e
                )))
            })?;
        }

        let original_path = self.output_path.clone();
        self.output_path = global_registry().resolve(&original_path).await;
        if self.output_path != original_path {
            info!(
                "Filename collision resolved: '{}' -> '{}'",
                original_path.display(),
                self.output_path.display()
            );
        }

        let release_path = |path: &std::path::Path| {
            let p = path.to_path_buf();
            #[allow(clippy::let_underscore_future)]
            let _ = tokio::spawn(async move {
                global_registry().release(&p).await;
            });
        };

        let url_for_head = reqwest::Url::parse(&uri).ok();
        let cookie_hdr_head = if let Some(ref url) = url_for_head {
            CookieHelper::new(Arc::clone(&self.cookie_storage), self.cookie_file.clone())
                .build_cookie_header_from_url(url)
        } else {
            String::new()
        };
        let mut head_req = self.client.head(&uri);
        if !cookie_hdr_head.is_empty() {
            head_req = head_req.header("Cookie", &cookie_hdr_head);
        }
        for (name, value) in &self.headers {
            head_req = head_req.header(name, value);
        }
        let head_resp = head_req.send().await.ok();
        let (total_length, head_supports_range) = if let Some(ref resp) = head_resp {
            let tl = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let sr = resp
                .headers()
                .get("Accept-Ranges")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.to_lowercase().contains("bytes"));
            (tl, sr)
        } else {
            (0, false)
        };

        let supports_range = if head_supports_range {
            true
        } else if total_length > constants::CONCURRENT_MIN_FILE_SIZE as u64 {
            let prober = RangeProber::new(Arc::clone(&self.client), self.headers.clone());
            prober.probe_range_support(&uri, total_length).await
        } else {
            false
        };

        let resume_helper = ResumeHelper::new(&self.output_path, true);
        let resume_state = resume_helper.detect(total_length).await?;

        // When resuming from a paused state, never short-circuit as "complete" —
        // the file on disk may be a preallocated sparse file that matches
        // total_length but hasn't actually been fully written. A download that
        // was explicitly paused by the user must always continue from where it
        // left off, relying on the control file's bitfield to determine which
        // ranges still need fetching.
        let was_paused = self.group.recover().is_paused_flag();

        if resume_state.is_complete && !was_paused {
            info!(
                "File already exists completely, skipping download: {} ({} bytes)",
                self.output_path.display(),
                resume_state.existing_length
            );
            self.completed_bytes = resume_state.existing_length;
            let g = self.group.recover();
            g.set_total_length(self.completed_bytes);
            g.update_progress(self.completed_bytes);
            g.set_completed_length(self.completed_bytes);
            drop(g);
            let mut g = self.group.recover_mut();
            g.complete()?;
            self.completed = true;
            release_path(&self.output_path);
            return Ok(());
        }

        // Final cancellation check before kicking off the (potentially long)
        // network transfer. If the task was removed during the HEAD probe or
        // resume detection, abort now rather than downloading data that will
        // just be discarded. This is placed before `spawn_progress_aggregator`
        // so a cancelled task does not spawn an unnecessary aggregator task.
        // Release the registered output path so future downloads can reuse
        // the filename.
        if let Err(e) = self.check_cancelled() {
            release_path(&self.output_path);
            return Err(e);
        }

        self.spawn_progress_aggregator();

        let download_result: Result<()> = async {
            if total_length > 0 {
                file_allocation::preallocate_file(
                    &self.output_path,
                    total_length,
                    &self.file_allocation,
                    self.secure_falloc,
                )
                .await?;
            }

            let options = self.group.recover().options_arc();
            let split = options.split.unwrap_or(constants::DEFAULT_SPLIT);

            let cookie_helper = self.create_cookie_helper();
            let progress_updater = self.create_progress_updater();

            if self.should_use_concurrent(total_length, supports_range, split) {
                if resume_state.should_resume {
                    info!(
                        "Concurrent mode + resume: existing {} bytes, continuing from offset {}",
                        resume_state.existing_length, resume_state.start_offset
                    );
                }
                let max_retries = options.max_retries;
                let progress_arc = Arc::clone(&self.progress);
                let mut concurrent_downloader = ConcurrentDownloader::new(
                    Arc::clone(&self.client),
                    self.output_path.clone(),
                    self.headers.clone(),
                    cookie_helper.clone(),
                    progress_updater.clone(),
                    Arc::clone(&self.group),
                    progress_arc,
                    self.mmap_threshold,
                    self.file_allocation.clone(),
                );
                match concurrent_downloader.execute_with_retry(
                    &uri,
                    total_length,
                    &resume_state,
                    max_retries,
                ).await {
                    Ok(ConcurrentDownloadResult::Complete) => return Ok(()),
                    Ok(ConcurrentDownloadResult::Fallback { completed_ranges }) => {
                        warn!(
                            "Concurrent download falling back to sequential mode, preserving {} completed ranges",
                            completed_ranges.len()
                        );
                        let retry_policy = RetryPolicy::new(options.max_retries, options.retry_wait * 1000);
                        let mut sequential_downloader = SequentialDownloader::new(
                            Arc::clone(&self.client),
                            self.output_path.clone(),
                            self.headers.clone(),
                            cookie_helper,
                            progress_updater,
                            Arc::clone(&self.group),
                            Arc::clone(&self.progress),
                        );
                        return sequential_downloader.execute_with_gaps_with_retry(
                            &uri,
                            total_length,
                            &completed_ranges,
                            &retry_policy,
                        ).await;
                    }
                    Err(e) => return Err(e),
                }
            }

            let retry_policy = RetryPolicy::new(options.max_retries, options.retry_wait * 1000);
            let mut sequential_downloader = SequentialDownloader::new(
                Arc::clone(&self.client),
                self.output_path.clone(),
                self.headers.clone(),
                cookie_helper,
                progress_updater,
                Arc::clone(&self.group),
                Arc::clone(&self.progress),
            );
            sequential_downloader.execute_with_retry(
                &uri,
                &resume_state,
                total_length,
                &retry_policy,
            ).await
        }
        .await;

        self.drain_progress_aggregator().await;

        if download_result.is_ok() {
            // Verify checksum if configured
            // Extract checksum config before any .await to avoid holding std::sync::RwLockReadGuard across await points
            let checksum_config = {
                let g = self.group.recover();
                g.options().checksum.clone()
            };
            if let Some((ref algo, ref expected)) = checksum_config
                && let Some(ht) = HashType::from_str(algo)
            {
                let cs = Checksum::new(ht, expected)?;
                let file = tokio::fs::File::open(&self.output_path)
                    .await
                    .map_err(|e| {
                        Aria2Error::Io(format!(
                            "Failed to open file for checksum verification: {}",
                            e
                        ))
                    })?;
                let mut reader = tokio::io::BufReader::with_capacity(65536, file);
                let mut validator = cs.create_validator();
                let mut buf = vec![0u8; 65536];
                loop {
                    let n = reader.read(&mut buf).await.map_err(|e| {
                        Aria2Error::Io(format!(
                            "Read error during checksum verification: {}",
                            e
                        ))
                    })?;
                    if n == 0 {
                        break;
                    }
                    validator.update(&buf[..n]);
                }
                if !validator.finalize()? {
                    tracing::error!(
                        algo = %algo,
                        path = %self.output_path.display(),
                        "Checksum mismatch"
                    );
                    return Err(Aria2Error::Checksum(format!(
                        "{} checksum mismatch for {}",
                        algo,
                        self.output_path.display()
                    )));
                }
                tracing::info!(
                    algo = %algo,
                    path = %self.output_path.display(),
                    "Checksum verified successfully"
                );
            }
            self.completed = true;
            let g = self.group.recover();
            let total = g.total_length();
            g.update_progress(total);
            g.set_completed_length(total);
        }
        release_path(&self.output_path);
        download_result
    }

    fn status(&self) -> CommandStatus {
        if self.completed {
            CommandStatus::Completed
        } else if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(
            constants::HTTP_DEFAULT_COMMAND_TIMEOUT_SECS,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

    impl DownloadCommand {
        fn has_progress_sender(&self) -> bool {
            self.progress_sender.is_some()
        }

        fn has_progress_receiver(&self) -> bool {
            self.progress_receiver.is_some()
        }

        fn has_progress_aggregator_handle(&self) -> bool {
            self.progress_aggregator_handle.is_some()
        }

        fn send_progress_update(&self, update: ProgressUpdate) {
            if let Some(ref sender) = self.progress_sender {
                let _ = sender.send(update);
            } else {
                panic!("test called send_progress_update but no sender is set");
            }
        }
    }

    #[test]
    fn test_progress_channel_auto_created() {
        let cmd = DownloadCommand::new(
            GroupId::new(1),
            "http://example.com/file.bin",
            &DownloadOptions::default(),
            None,
            None,
        )
        .expect("DownloadCommand::new should succeed with a valid HTTP URI");

        assert!(
            cmd.has_progress_sender(),
            "progress_sender should be Some after construction (auto-created)"
        );
        assert!(
            cmd.has_progress_receiver(),
            "progress_receiver should be Some after construction (held for lazy spawn)"
        );
        assert!(
            !cmd.has_progress_aggregator_handle(),
            "progress_aggregator_handle should be None until execute() spawns it"
        );
    }

    #[tokio::test]
    async fn test_progress_updates_flow_through_channel() {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(2),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));
        let group_clone = Arc::clone(&group);

        let mut cmd = DownloadCommand::new_with_group(
            group,
            "http://example.com/file.bin",
            &DownloadOptions::default(),
            None,
            None,
        )
        .expect("DownloadCommand::new_with_group should succeed");

        assert!(cmd.has_progress_sender());
        assert!(cmd.has_progress_receiver());

        cmd.spawn_progress_aggregator();
        assert!(cmd.has_progress_aggregator_handle());
        assert!(!cmd.has_progress_receiver());

        cmd.send_progress_update(ProgressUpdate {
            completed_bytes: 4096,
            download_speed: 0,
            upload_speed: 0,
        });

        cmd.drain_progress_aggregator().await;
        assert!(!cmd.has_progress_sender());
        assert!(!cmd.has_progress_aggregator_handle());

        let completed = { group_clone.recover().get_completed_length() };
        assert_eq!(
            completed, 4096,
            "aggregator should have applied the progress update to RequestGroup"
        );
    }

    /// Verify that `check_cancelled()` returns `Ok(())` for a fresh group
    /// (status = Waiting) and `Err(DownloadFailed)` after the group is
    /// marked `Removed`.
    #[tokio::test]
    async fn test_check_cancelled_returns_ok_for_active_group() {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(10),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));

        let cmd = DownloadCommand::new_with_group(
            group,
            "http://example.com/file.bin",
            &DownloadOptions::default(),
            None,
            None,
        )
        .expect("DownloadCommand::new_with_group should succeed");

        // Fresh group (Waiting status) — not cancelled.
        assert!(
            cmd.check_cancelled().is_ok(),
            "check_cancelled() should return Ok for a fresh (non-removed) group"
        );
    }

    #[tokio::test]
    async fn test_check_cancelled_returns_err_after_remove() {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(11),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));

        let cmd = DownloadCommand::new_with_group(
            Arc::clone(&group),
            "http://example.com/file.bin",
            &DownloadOptions::default(),
            None,
            None,
        )
        .expect("DownloadCommand::new_with_group should succeed");

        // Simulate `aria2.remove` / `aria2.forceRemove` which calls
        // RequestGroupMan::remove_group -> group.remove().
        {
            let mut g = group.recover_mut();
            g.remove().unwrap();
        }

        let err = cmd.check_cancelled().expect_err(
            "check_cancelled() should return Err after the group is marked Removed",
        );
        assert!(
            matches!(err, Aria2Error::DownloadFailed(_)),
            "expected DownloadFailed error, got {:?}",
            err
        );
    }
}
