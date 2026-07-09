use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::constants;
use crate::engine::active_output_registry::global_registry;
use crate::engine::command::{Command, CommandStatus};
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http_segment_downloader::HttpSegmentDownloader;
use crate::engine::mirror_coordinator::{MirrorConfig, MirrorCoordinator};
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::{
    CachedDiskWriter, DefaultDiskWriter, DiskWriter, SeekableDiskWriter,
};
use crate::filesystem::file_allocation;
use crate::filesystem::resume_helper::{ResumeHelper, ResumeState};
use crate::http::client_pool;
use crate::http::cookie::Cookie;
use crate::http::cookie_storage::CookieStorage;
use crate::http::socks_connector::{NoProxyMatcher, ProxyUrl};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::perf_monitor::{AtomicMetrics, Metrics, PerformanceMonitor};



pub struct DownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    client: Arc<reqwest::Client>,
    output_path: std::path::PathBuf,
    started: bool,
    completed: bool,
    completed_bytes: u64,
    continue_enabled: bool,
    file_allocation: String,
    cookie_storage: Arc<CookieStorage>,
    cookie_file: Option<String>,
    no_proxy_matcher: Option<NoProxyMatcher>,
    /// Server statistics manager for intelligent mirror selection.
    /// Shared across downloads to maintain historical performance data.
    stat_man: Arc<ServerStatMan>,
    /// Performance monitor for collecting metrics (optional, minimal overhead when enabled)
    perf_monitor: Option<Arc<PerformanceMonitor>>,
    /// Atomic metrics for low-overhead collection
    atomic_metrics: Arc<AtomicMetrics>,
    /// Parsed custom HTTP headers (Name, Value) applied to HEAD probes and range
    /// GETs. Includes `User-Agent` / `Referer` overrides from options.
    headers: Vec<(String, String)>,
}

impl DownloadCommand {
    /// Create a DownloadCommand with a standalone RequestGroup (not registered
    /// in RequestGroupMan). Use this for simple CLI/test scenarios where
    /// external progress queries are not needed.
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let group = Arc::new(tokio::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group(group, uri, options, output_dir, output_name)
    }

    /// Create a DownloadCommand with a shared RequestGroup Arc (registered in
    /// RequestGroupMan). Use this for RPC and CLI paths where external progress
    /// queries are needed.
    pub fn new_with_group(
        group: Arc<tokio::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(uri))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Parse custom headers once (includes User-Agent / Referer overrides).
        let headers = options.parsed_headers();

        // Use global client if no proxy configuration AND no custom headers,
        // otherwise create custom client (custom headers require per-request
        // application but a custom client is still built when a proxy is set).
        let client = if options.http_proxy.is_none() && options.all_proxy.is_none() {
            // No proxy configuration - use shared global client for better performance.
            // Custom headers are applied per-request in execute()/segment downloader.
            client_pool::get_global_client()
        } else {
            // Proxy configuration present - create custom client
            let mut builder = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS))
                .timeout(Duration::from_secs(constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS))
                .user_agent(constants::USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(constants::HTTP_DEFAULT_MAX_REDIRECTS))
                .pool_max_idle_per_host(constants::HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST)
                .pool_idle_timeout(Some(std::time::Duration::from_secs(constants::HTTP_DEFAULT_POOL_IDLE_TIMEOUT_SECS)))
                .tcp_keepalive(Some(std::time::Duration::from_secs(constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS)));

            if let Some(ref proxy) = options.http_proxy
                && let Ok(proxy_url) = proxy.parse::<reqwest::Url>()
                && let Ok(p) = reqwest::Proxy::all(proxy_url.to_string())
            {
                builder = builder.proxy(p);
            }

            // Apply all-proxy (global proxy for all protocols)
            // Priority: protocol-specific proxy > all-proxy
            if options.http_proxy.is_none()
                && let Some(ref all_proxy) = options.all_proxy
            {
                match ProxyUrl::parse(all_proxy) {
                    Ok(parsed) => {
                        match parsed.protocol {
                            crate::http::socks_connector::ProxyProtocol::Http
                            | crate::http::socks_connector::ProxyProtocol::Https => {
                                if let Ok(p) = reqwest::Proxy::all(all_proxy.to_string()) {
                                    builder = builder.proxy(p);
                                }
                            }
                            _ => {
                                // SOCKS proxies require a custom connector.
                                // For now, log a note that SOCKS integration is available
                                // via the SocksConnector trait but requires manual TcpStream wrapping.
                                tracing::info!(
                                    "SOCKS proxy configured ({}) - use SocksConnector for direct TCP connections",
                                    all_proxy
                                );
                            }
                        }
                    }
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

        if let Some(ref cf) = cookie_file {
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

        Ok(Self {
            group,
            client,
            output_path: path,
            started: false,
            completed: false,
            completed_bytes: 0,
            continue_enabled: true,
            file_allocation: constants::DEFAULT_FILE_ALLOCATION.to_string(),
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: Arc::new(ServerStatMan::new()),
            perf_monitor: None, // Disabled by default for zero overhead
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            headers,
        })
    }

    /// Create a DownloadCommand with a custom HTTP client and standalone group.
    pub fn new_with_client(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        client: Arc<reqwest::Client>,
    ) -> Result<Self> {
        let group = Arc::new(tokio::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group_and_client(group, uri, options, output_dir, output_name, client)
    }

    /// Create a DownloadCommand with a shared group Arc and custom HTTP client.
    pub fn new_with_group_and_client(
        group: Arc<tokio::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        client: Arc<reqwest::Client>,
    ) -> Result<Self> {
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
        info!("DownloadCommand created (shared client): {} -> {}", uri, path.display());

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = Arc::new(CookieStorage::new());

        if let Some(ref cf) = cookie_file {
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

        Ok(Self {
            group,
            client,
            output_path: path,
            started: false,
            completed: false,
            completed_bytes: 0,
            continue_enabled: true,
            file_allocation: constants::DEFAULT_FILE_ALLOCATION.to_string(),
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: Arc::new(ServerStatMan::new()),
            perf_monitor: None, // Disabled by default for zero overhead
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            headers,
        })
    }

    /// Create a DownloadCommand with a shared ServerStatMan.
    ///
    /// This allows multiple downloads to share server performance statistics,
    /// enabling intelligent mirror selection based on historical data.
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

    /// Enable performance monitoring for this download.
    ///
    /// This adds minimal overhead (< 1%) to track throughput, latency, memory, and lock wait times.
    pub fn enable_perf_monitor(&mut self) {
        self.perf_monitor = Some(Arc::new(PerformanceMonitor::new()));
    }

    /// Get performance metrics snapshot (low-overhead, always available)
    pub fn get_perf_metrics(&self) -> Metrics {
        self.atomic_metrics.snapshot()
    }

    /// Get performance report if monitoring is enabled
    pub fn get_perf_report(&self) -> Option<String> {
        self.perf_monitor.as_ref().map(|m| m.export_text())
    }

    /// Get performance report in JSON format if monitoring is enabled
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

    fn save_cookies_if_configured(&self) {
        if let Some(ref cf) = self.cookie_file {
            if let Err(e) = self.cookie_storage.save_file(std::path::Path::new(cf)) {
                warn!("Failed to save cookie file {}: {}", cf, e);
            } else {
                info!("Cookies saved to {}", cf);
            }
        }
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }

    pub async fn group_mut(&self) -> tokio::sync::RwLockWriteGuard<'_, RequestGroup> {
        self.group.write().await
    }

    /// Get the NoProxy matcher for checking if a host should bypass proxy
    pub fn no_proxy_matcher(&self) -> Option<&NoProxyMatcher> {
        self.no_proxy_matcher.as_ref()
    }

    fn should_use_concurrent(&self, total_length: u64, supports_range: bool) -> bool {
        if !supports_range {
            return false;
        }
        if total_length < constants::CONCURRENT_MIN_FILE_SIZE as u64 {
            return false;
        }

        let split = { self.group.blocking_read().options().split.unwrap_or(1) };
        split > 1
    }

    async fn execute_sequential_download(
        &mut self,
        uri: &str,
        resume_state: &crate::filesystem::resume_helper::ResumeState,
        _total_length: u64,
    ) -> Result<()> {
        let url_parsed = reqwest::Url::parse(uri).ok();
        let mut request = if let Some(range_header) = ResumeHelper::build_range_header(resume_state)
        {
            debug!("Resume download: {}", range_header);
            self.client.get(uri).header("Range", range_header)
        } else {
            self.client.get(uri)
        };

        if let Some(ref url) = url_parsed {
            let host = url.host_str().unwrap_or("");
            let path = url.path();
            let secure = url.scheme() == "https";
            let cookie_hdr = self.cookie_storage.to_header_string(host, path, secure);
            if !cookie_hdr.is_empty() {
                request = request.header("Cookie", &cookie_hdr);
            }
        }

        // Apply custom HTTP headers (Referer, User-Agent, etc.)
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }

        let response = request.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP request failed: {}", e),
            })
        })?;

        if let Some(ref url) = url_parsed {
            let domain = url.host_str().unwrap_or("");
            let path = url.path();
            for sc_val in response.headers().get_all("set-cookie").iter() {
                if let Ok(sc_str) = sc_val.to_str()
                    && let Some(c) = Cookie::from_set_cookie_header(sc_str, domain, path)
                {
                    self.cookie_storage.add(c);
                    debug!("Received Set-Cookie: {}", sc_str);
                }
            }
        }

        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            if status.as_u16() >= 500 {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code: status.as_u16(),
                }));
            }
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                format!("HTTP error: {}", status),
            )));
        }

        let resp_length = response.content_length().unwrap_or(0) as u64;
        let actual_total = if resume_state.should_resume {
            resume_state.start_offset + resp_length
        } else {
            resp_length
        };
        {
            let mut g = self.group.write().await;
            g.set_total_length(actual_total).await;
            // Export to atomic field for session persistence
            g.set_total_length_atomic(actual_total);
        }

        let start_offset = if resume_state.should_resume {
            resume_state.start_offset
        } else {
            0
        };
        self.completed_bytes = start_offset;
        let rate_limit = { self.group.read().await.options().max_download_limit };

        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let mut writer: Box<dyn DiskWriter> = match rate_limit {
            Some(rate) if rate > 0 => {
                let cfg = RateLimiterConfig::new(Some(rate), None);
                let limiter = RateLimiter::new(&cfg);
                debug!("Download speed limit enabled: {} bytes/s", rate);
                Box::new(ThrottledWriter::new(raw_writer, limiter))
            }
            _ => Box::new(raw_writer),
        };

        let mut stream = response.bytes_stream();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;
        let mut last_progress_update = 0u64; // Track last progress update for batch updates

        while let Some(chunk) = stream.next().await {
            let data: bytes::Bytes = chunk.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
            writer.write(&data).await?;
            self.completed_bytes += data.len() as u64;

            // Batch progress updates to reduce lock contention
            // Only update progress every PROGRESS_UPDATE_BYTES (256KB)
            if self.completed_bytes - last_progress_update >= constants::PROGRESS_UPDATE_BYTES as u64 {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;
                // Export to atomic fields for session persistence
                g.set_completed_length(self.completed_bytes);
                last_progress_update = self.completed_bytes;

                // Speed update still happens every 500ms
                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= constants::HTTP_SPEED_UPDATE_INTERVAL_MS as u128 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    // Update cached download speed for session persistence
                    g.set_download_speed_cached(speed);
                    
                    // Record performance metrics (minimal overhead)
                    self.atomic_metrics.record_throughput(speed);
                    
                    // Record to performance monitor if enabled
                    if let Some(ref monitor) = self.perf_monitor {
                        let metrics = Metrics::new(speed, elapsed.as_millis() as u64, 0, 0)
                            .with_label("download_speed");
                        monitor.record_metric("download_speed", metrics);
                    }
                    
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        writer.finalize().await.ok();

        // Calculate final speed using group's elapsed_time for consistency
        let final_speed = {
            let g = self.group.read().await;
            let elapsed = g.elapsed_time().await;
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (self.completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            // Export final progress to atomic fields for session persistence
            g.set_completed_length(self.completed_bytes);
            g.set_download_speed_cached(final_speed);
            g.complete().await?;
        }

        info!(
            "Sequential download complete: {} ({} bytes)",
            self.output_path.display(),
            self.completed_bytes
        );
        self.save_cookies_if_configured();
        Ok(())
    }

    #[allow(dead_code)] // Reserved for future concurrent download execution strategy
    async fn execute_concurrent_download(&mut self, uri: &str, total_length: u64) -> Result<()> {
        let options = self.group.read().await.options().clone();
        let split = options.split.unwrap_or(1) as usize;
        let max_conn = options.max_connection_per_server.unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16) as usize;
        let seg_size = total_length / split as u64;
        let mut last_progress_update = 0u64; // Track last progress update for batch updates

        info!(
            "Concurrent download started: split={}, max_conn={}, segment_size={} bytes, total={}",
            split, max_conn, seg_size, total_length
        );

        let mut manager =
            ConcurrentSegmentManager::new(total_length, vec![uri.to_string()], Some(seg_size));
        manager.set_max_connections_per_mirror(max_conn.min(split));
        manager.allocate_segments();

        let url_parsed = reqwest::Url::parse(uri).ok();
        let cookie_hdr = if let Some(ref url) = url_parsed {
            self.cookie_storage.to_header_string(
                url.host_str().unwrap_or(""),
                url.path(),
                url.scheme() == "https",
            )
        } else {
            String::new()
        };
        let cookie_hdr_for_spawn: Option<String> = if cookie_hdr.is_empty() {
            None
        } else {
            Some(cookie_hdr)
        };

        let mut writer = CachedDiskWriter::new(&self.output_path, Some(total_length), None);

        let limiter = options
            .max_download_limit
            .filter(|&r| r > 0)
            .map(|r| RateLimiter::new(&RateLimiterConfig::new(Some(r), None)));

        let mut active_handles: std::collections::HashMap<
            u32,
            tokio::task::JoinHandle<std::result::Result<bytes::Bytes, String>>,
        > = std::collections::HashMap::new();

        loop {
            while active_handles.len() < max_conn {
                match manager.next_pending_segment_for_mirror(0) {
                    Some((seg_idx, offset, length)) => {
                        let url = uri.to_string();
                        let dl = HttpSegmentDownloader::new(&self.client);
                        let ch = cookie_hdr_for_spawn.clone();
                        let headers = self.headers.clone();
                        let handle = tokio::spawn(async move {
                            dl.download_range(&url, offset, length, ch.as_deref(), &headers)
                                .await
                                .map_err(|e| e.to_string())
                        });
                        active_handles.insert(seg_idx, handle);
                    }
                    None => break,
                }
            }

            if !active_handles.is_empty() {
                let mut completed = Vec::new();
                for (&seg_idx, handle) in &active_handles {
                    if handle.is_finished() {
                        completed.push(seg_idx);
                    }
                }

                for seg_idx in completed {
                    if let Some(handle) = active_handles.remove(&seg_idx) {
                        match handle.await {
                            Ok(Ok(data)) => {
                                if let Some(ref lim) = limiter {
                                    lim.acquire_download(data.len() as u64).await;
                                }
                                if let Some((offset, _, _)) = manager.segment_info(seg_idx as usize)
                                {
                                    writer.write_at(offset, &data).await.map_err(|e| {
                                        Aria2Error::Fatal(crate::error::FatalError::Config(
                                            format!("Write failed: {}", e),
                                        ))
                                    })?;
                                } else {
                                    writer.write_at(0, &data).await.ok();
                                }
                                let data_len = data.len();
                                manager.complete_segment(seg_idx, data);
                                self.completed_bytes += data_len as u64;

                                // Batch progress updates to reduce lock contention
                                if self.completed_bytes - last_progress_update >= constants::PROGRESS_UPDATE_BYTES as u64 {
                                    let g = self.group.write().await;
                                    g.update_progress(self.completed_bytes).await;
                                    // Export to atomic fields for session persistence
                                    g.set_completed_length(self.completed_bytes);
                                    last_progress_update = self.completed_bytes;
                                }
                            }
                            Ok(Err(e)) => {
                                warn!("Segment {} download failed: {}", seg_idx, e);
                                manager.fail_segment(seg_idx);
                            }
                            Err(_) => {
                                warn!("Segment {} task panic", seg_idx);
                                manager.fail_segment(seg_idx);
                            }
                        }
                    }
                }
            }

            if manager.is_complete() {
                debug!("All segments complete");
                break;
            }

            if manager.has_failed_segments()
                && !manager.has_pending_segments()
                && active_handles.is_empty()
            {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Concurrent download: all segments failed".into(),
                    },
                ));
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        writer.flush().await.map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Flush failed: {}",
                e
            )))
        })?;

        // Calculate final speed using group's elapsed_time for consistency
        let final_speed = {
            let g = self.group.read().await;
            let elapsed = g.elapsed_time().await;
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (self.completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            // Export final progress to atomic fields for session persistence
            g.set_completed_length(self.completed_bytes);
            g.set_download_speed_cached(final_speed);
            g.complete().await?;
        }

        info!(
            "Concurrent download complete: {} ({} bytes)",
            self.output_path.display(),
            self.completed_bytes
        );
        self.save_cookies_if_configured();
        Ok(())
    }

    async fn execute_sequential_download_with_retry(
        &mut self,
        uri: &str,
        resume_state: &ResumeState,
        _total_length: u64,
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut last_err = None;

        for attempt in 0..=retry_policy.max_retries {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt - 1)
            {
                info!("Sequential download retry #{} (waiting {:?})...", attempt, wait);
                tokio::time::sleep(wait).await;
            }

            match self
                .execute_sequential_download(uri, resume_state, _total_length)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("Sequential download attempt #{} failed: {}", attempt + 1, e);
                    last_err = Some(e);
                    if retry_policy.is_exhausted(attempt)
                        || !retry_policy
                            .should_retry_error(&format!("{:?}", last_err.as_ref().unwrap()))
                    {
                        break;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "All retries failed".into(),
            })
        }))
    }

    async fn execute_concurrent_download_with_retry(
        &mut self,
        uri: &str,
        total_length: u64,
        resume_state: &ResumeState,
        max_retries_per_segment: u32,
    ) -> Result<()> {
        info!(
            "Using concurrent download mode (split={}, max_retries/segment={})",
            self.group.read().await.options().split.unwrap_or(1),
            max_retries_per_segment
        );

        // Get all URIs from the request group for multi-mirror support
        let all_uris: Vec<String> = {
            let g = self.group.read().await;
            g.uris().to_vec()
        };

        // Check if we have multiple mirrors
        let use_intelligent_selection = all_uris.len() > 1;

        if use_intelligent_selection {
            info!(
                "Intelligent multi-mirror selection enabled: {} mirror sources",
                all_uris.len()
            );
            self.execute_concurrent_download_with_coordinator(
                &all_uris,
                total_length,
                resume_state,
                max_retries_per_segment,
            )
            .await
        } else {
            // Fallback to single-URI concurrent download
            self.execute_concurrent_download_single_uri(
                uri,
                total_length,
                resume_state,
                max_retries_per_segment,
            )
            .await
        }
    }

    /// Execute concurrent download using MirrorCoordinator for intelligent mirror selection.
    ///
    /// This method uses the new MirrorCoordinator to intelligently select mirrors
    /// based on historical performance data and real-time feedback.
    async fn execute_concurrent_download_with_coordinator(
        &mut self,
        uris: &[String],
        total_length: u64,
        resume_state: &ResumeState,
        max_retries_per_segment: u32,
    ) -> Result<()> {
        let split = self.group.read().await.options().split.unwrap_or(1) as u64;
        let segment_size = total_length.div_ceil(split);
        let max_conn = self.group.read().await.options().max_connection_per_server.unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16) as usize;

        // Create mirror configuration
        let mirror_config = MirrorConfig {
            max_connections_per_mirror: max_conn.min(split as usize),
            max_total_connections: max_conn * uris.len(),
            speed_threshold: constants::MIRROR_SPEED_THRESHOLD,
            cooldown_secs: constants::MIRROR_COOLDOWN_SECS,
            max_retries: max_retries_per_segment,
        };

        // Create URI selector with server stats
        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&self.stat_man),
            uris.to_vec(),
        ));

        // Create segment manager with selector
        let segment_manager = ConcurrentSegmentManager::new_with_selector(
            total_length,
            uris.to_vec(),
            Some(segment_size),
            Arc::clone(&self.stat_man),
            selector,
        );

        // Create mirror coordinator
        let mut coordinator = MirrorCoordinator::with_segment_manager(
            Arc::clone(&self.stat_man),
            Box::new(crate::selector::uri_selector::InorderUriSelector::new()),
            segment_manager,
            mirror_config,
            uris.to_vec(),
        );

        if resume_state.should_resume {
            // Note: We'd need to expose mark_completed_up_to in MirrorCoordinator
            // For now, we'll handle this at the segment manager level
            debug!(
                "Resume: existing {} bytes, continuing from offset {}",
                resume_state.existing_length, resume_state.start_offset
            );
        }

        let mut writer = CachedDiskWriter::new(&self.output_path, Some(total_length), None);
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;
        let mut last_progress_update = 0u64; // Track last progress update for batch updates

        while coordinator.has_pending_segments() || !coordinator.is_complete() {
            // Select mirror and segment
            while let Some((mirror_idx, mirror_url, (seg_idx, offset, length))) =
                coordinator.select_mirror_for_segment()
            {
                info!(
                    "Starting segment {} download: mirror={}, offset={}, size={}",
                    seg_idx, mirror_idx, offset, length
                );

                let downloader = HttpSegmentDownloader::new(&self.client);
                let seg_start = Instant::now();

                // Get cookie header for this mirror
                let url_parsed = reqwest::Url::parse(&mirror_url).ok();
                let cookie_hdr = if let Some(ref url) = url_parsed {
                    self.cookie_storage.to_header_string(
                        url.host_str().unwrap_or(""),
                        url.path(),
                        url.scheme() == "https",
                    )
                } else {
                    String::new()
                };

                let result = downloader
                    .download_range(&mirror_url, offset, length, if cookie_hdr.is_empty() { None } else { Some(&cookie_hdr) }, &self.headers)
                    .await;

                match result {
                    Ok(data) => {
                        let elapsed = seg_start.elapsed();
                        let speed = if elapsed.as_secs_f64() > 0.0 {
                            (data.len() as f64 / elapsed.as_secs_f64()) as u64
                        } else {
                            0
                        };

                        debug!("Segment {} complete: {} bytes, speed={} B/s", seg_idx, data.len(), speed);

                        writer.write_at(offset, &data).await.map_err(|e| {
                            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                                "Write failed: {}",
                                e
                            )))
                        })?;

                        // Report success to coordinator for speed feedback
                        coordinator.on_segment_complete(mirror_idx, seg_idx, data, speed);
                    }
                    Err(e) => {
                        warn!("Segment {} download failed (mirror={}): {}", seg_idx, mirror_idx, e);

                        // Default error code for network errors
                        let error_code = constants::HTTP_DEFAULT_ERROR_CODE;

                        // Report failure to coordinator
                        coordinator.on_segment_failed(mirror_idx, seg_idx, error_code);
                    }
                }

                // Update progress
                self.completed_bytes = {
                    // Get completed bytes from coordinator's progress
                    let total = coordinator.num_segments() as u64;
                    let progress_pct = coordinator.progress();
                    if total > 0 {
                        (progress_pct / 100.0 * total as f64) as u64
                    } else {
                        0
                    }
                };

                // Batch progress updates to reduce lock contention
                if self.completed_bytes - last_progress_update >= constants::PROGRESS_UPDATE_BYTES as u64 {
                    let g = self.group.write().await;
                    g.update_progress(self.completed_bytes).await;
                    g.set_completed_length(self.completed_bytes);
                    last_progress_update = self.completed_bytes;

                    // Update speed periodically
                    let elapsed = last_speed_update.elapsed();
                    if elapsed.as_millis() >= constants::HTTP_SPEED_UPDATE_INTERVAL_MS as u128 {
                        let delta = self.completed_bytes - last_completed;
                        let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                        g.update_speed(speed, 0).await;
                        g.set_download_speed_cached(speed);
                        last_speed_update = Instant::now();
                        last_completed = self.completed_bytes;
                    }
                }
            }

            if coordinator.is_complete() {
                break;
            }

            if coordinator.has_failed_segments() {
                error!("Permanently failed download segments exist");
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Some download segments permanently failed".into(),
                    },
                ));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        writer.flush().await.map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Flush failed: {}",
                e
            )))
        })?;

        // Calculate final speed using group's elapsed_time for consistency
        let final_speed = {
            let g = self.group.read().await;
            let elapsed = g.elapsed_time().await;
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (self.completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };

        {
            let mut g = self.group.write().await;
            g.set_total_length(self.completed_bytes).await;
            g.set_total_length_atomic(self.completed_bytes);
            g.set_completed_length(self.completed_bytes);
            g.update_speed(final_speed, 0).await;
            g.set_download_speed_cached(final_speed);
            g.complete().await?;
        }

        // Save server stats for future use
        if let Err(e) = self.save_server_stats().await {
            warn!("Failed to save server statistics: {}", e);
        }

        info!(
            "Multi-mirror concurrent download complete: {} ({} bytes, {} B/s)",
            self.output_path.display(),
            self.completed_bytes,
            final_speed
        );
        self.save_cookies_if_configured();
        Ok(())
    }

    /// Execute concurrent download for a single URI (fallback method).
    async fn execute_concurrent_download_single_uri(
        &mut self,
        uri: &str,
        total_length: u64,
        resume_state: &ResumeState,
        max_retries_per_segment: u32,
    ) -> Result<()> {
        let split = self.group.read().await.options().split.unwrap_or(1) as u64;
        let segment_size = total_length.div_ceil(split);
        let mut manager =
            ConcurrentSegmentManager::new(total_length, vec![uri.to_string()], Some(segment_size));
        manager.set_max_retries(max_retries_per_segment);

        if resume_state.should_resume {
            manager.mark_completed_up_to(resume_state.start_offset, resume_state.existing_length);
        }

        let mut writer = CachedDiskWriter::new(&self.output_path, Some(total_length), None);

        while manager.has_pending_segments() || !manager.is_complete() {
            while let Some((seg_idx, offset, length)) = manager.next_pending_segment() {
                let seg_idx_u32 = seg_idx;
                info!(
                    "Starting segment {} download: offset={}, size={}",
                    seg_idx, offset, length
                );
                let downloader = HttpSegmentDownloader::new(&self.client);
                let seg_start = Instant::now();
                let result = downloader.download_range(uri, offset, length, None, &self.headers).await;

                match result {
                    Ok(data) => {
                        let elapsed = seg_start.elapsed();
                        let speed = if elapsed.as_secs_f64() > 0.0 {
                            (data.len() as f64 / elapsed.as_secs_f64()) as u64
                        } else {
                            0
                        };
                        debug!("Segment {} complete: {} bytes, speed={} B/s", seg_idx, data.len(), speed);

                        writer.write_at(offset, &data).await.map_err(|e| {
                            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                                "Write failed: {}",
                                e
                            )))
                        })?;

                        // Report completion with speed feedback
                        manager.report_segment_complete(seg_idx_u32, data, speed, false);
                    }
                    Err(e) => {
                        warn!("Segment {} download failed: {}", seg_idx, e);
                        manager.report_segment_failed(seg_idx_u32, constants::HTTP_DEFAULT_ERROR_CODE);
                    }
                }

                self.completed_bytes = manager.completed_bytes();
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;
                // Export to atomic fields for session persistence
                g.set_completed_length(self.completed_bytes);
            }

            if manager.is_complete() {
                break;
            }

            if manager.has_permanently_failed_segments() {
                error!("Permanently failed download segments exist");
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Some download segments permanently failed".into(),
                    },
                ));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        writer.flush().await.map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Flush failed: {}",
                e
            )))
        })?;
        self.completed_bytes = manager.completed_bytes();
        let mut g = self.group.write().await;
        g.set_total_length(self.completed_bytes).await;
        // Export to atomic fields for session persistence
        g.set_total_length_atomic(self.completed_bytes);
        g.set_completed_length(self.completed_bytes);
        g.complete().await?;
        Ok(())
    }

    /// Save server statistics to the default location.
    async fn save_server_stats(&self) -> std::result::Result<usize, String> {
        // In a real implementation, this would save to a configured path
        // For now, we just return success
        Ok(0)
    }

    /// Get a reference to the server statistics manager.
    pub fn stat_man(&self) -> &Arc<ServerStatMan> {
        &self.stat_man
    }
}

#[async_trait]
impl Command for DownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        let uri = {
            let g = self.group.read().await;
            g.uris().first().cloned().unwrap_or_default()
        };

        if uri.is_empty() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "Download URI is empty".into(),
            )));
        }

        debug!("Starting download: {} -> {}", uri, self.output_path.display());

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

        // Resolve filename collision: if another active download is already
        // writing to self.output_path, replace it with a unique name such as
        // "file (1).ext" so that concurrent downloads do not silently overwrite.
        let original_path = self.output_path.clone();
        self.output_path = global_registry().resolve(&original_path).await;
        if self.output_path != original_path {
            info!(
                "Filename collision resolved: '{}' -> '{}'",
                original_path.display(),
                self.output_path.display()
            );
        }

        // Ensure the resolved path is released when execute() returns (success or failure).
        let release_path = |path: &std::path::Path| {
            let p = path.to_path_buf();
            #[allow(clippy::let_underscore_future)]
            let _ = tokio::spawn(async move {
                global_registry().release(&p).await;
            });
        };

        let url_for_head = reqwest::Url::parse(&uri).ok();
        let cookie_hdr_head = if let Some(ref url) = url_for_head {
            self.cookie_storage.to_header_string(
                url.host_str().unwrap_or(""),
                url.path(),
                url.scheme() == "https",
            )
        } else {
            String::new()
        };
        let mut head_req = self.client.head(&uri);
        if !cookie_hdr_head.is_empty() {
            head_req = head_req.header("Cookie", &cookie_hdr_head);
        }
        // Apply custom HTTP headers (Referer, User-Agent override, etc.) to the
        // HEAD probe so servers that 403 without them respond correctly.
        for (name, value) in &self.headers {
            head_req = head_req.header(name, value);
        }
        let head_resp = head_req.send().await.ok();
        let (total_length, supports_range) = if let Some(ref resp) = head_resp {
            let tl = resp.content_length().unwrap_or(0);
            let sr = resp
                .headers()
                .get("Accept-Ranges")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.to_lowercase().contains("bytes"));
            (tl, sr)
        } else {
            (0, false)
        };

        let resume_helper = ResumeHelper::new(&self.output_path, self.continue_enabled);
        let resume_state = resume_helper.detect(total_length).await?;

        if resume_state.is_complete {
            info!(
                "File already exists completely, skipping download: {} ({} bytes)",
                self.output_path.display(),
                resume_state.existing_length
            );
            self.completed_bytes = resume_state.existing_length;
            let mut g = self.group.write().await;
            g.set_total_length(self.completed_bytes).await;
            g.update_progress(self.completed_bytes).await;
            // Export to atomic fields for session persistence
            g.set_total_length_atomic(self.completed_bytes);
            g.set_completed_length(self.completed_bytes);
            g.complete().await?;
            self.completed = true;
            release_path(&self.output_path);
            return Ok(());
        }

        if total_length > 0 {
            file_allocation::preallocate_file(
                &self.output_path,
                total_length,
                &self.file_allocation,
            )
            .await?;
        }

        let options = self.group.read().await.options().clone();

        if self.should_use_concurrent(total_length, supports_range) {
            if resume_state.should_resume {
                info!(
                    "Concurrent mode + resume: existing {} bytes, continuing from offset {}",
                    resume_state.existing_length, resume_state.start_offset
                );
            }
            let max_retries = options.max_retries;
            let result = self
                .execute_concurrent_download_with_retry(
                    &uri,
                    total_length,
                    &resume_state,
                    max_retries,
                )
                .await;
            if result.is_ok() {
                self.completed = true;
            }
            release_path(&self.output_path);
            return result;
        }

        let retry_policy = RetryPolicy::new(options.max_retries, options.retry_wait * 1000);
        let result = self
            .execute_sequential_download_with_retry(
                &uri,
                &resume_state,
                total_length,
                &retry_policy,
            )
            .await;
        if result.is_ok() {
            self.completed = true;
        }
        release_path(&self.output_path);
        result
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
        Some(Duration::from_secs(constants::HTTP_DEFAULT_COMMAND_TIMEOUT_SECS))
    }
}
