use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http_segment_downloader::HttpSegmentDownloader;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::{
    CachedDiskWriter, DefaultDiskWriter, DiskWriter, SeekableDiskWriter,
};
use crate::filesystem::file_allocation;
use crate::filesystem::resume_helper::{ResumeHelper, ResumeState};
use crate::http::cookie::Cookie;
use crate::http::cookie_storage::CookieStorage;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

const CONCURRENT_MIN_FILE_SIZE: u64 = 1024 * 1024;

pub struct DownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    client: reqwest::Client,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    continue_enabled: bool,
    file_allocation: String,
    cookie_storage: Arc<CookieStorage>,
    cookie_file: Option<String>,
}

impl DownloadCommand {
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(uri))
            .unwrap_or_else(|| "download".to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .user_agent("aria2-rust/1.0")
            .redirect(reqwest::redirect::Policy::limited(5))
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(Some(std::time::Duration::from_secs(90)))
            .tcp_keepalive(Some(std::time::Duration::from_secs(60)));

        if let Some(ref proxy) = options.http_proxy {
            if let Ok(proxy_url) = proxy.parse::<reqwest::Url>() {
                if let Ok(p) = reqwest::Proxy::all(&proxy_url.to_string()) {
                    builder = builder.proxy(p);
                }
            }
        }

        let client = builder.build().map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "创建HTTP客户端失败: {}",
                e
            )))
        })?;

        let group = RequestGroup::new(gid, vec![uri.to_string()], options.clone());
        info!("DownloadCommand 创建: {} -> {}", uri, path.display());

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = Arc::new(CookieStorage::new());

        if let Some(ref cf) = cookie_file {
            let p = std::path::Path::new(cf);
            if p.exists() {
                match cookie_storage.load_file(p) {
                    Ok(n) => info!("从文件加载了 {} 个 Cookie: {}", n, cf),
                    Err(e) => warn!("加载 Cookie 文件失败 {}: {}", cf, e),
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
                info!("手动设置了 {} 个 Cookie", cookie_storage.count());
            }
        }

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            client,
            output_path: path,
            started: false,
            completed_bytes: 0,
            continue_enabled: true,
            file_allocation: "prealloc".to_string(),
            cookie_storage,
            cookie_file,
        })
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
            .unwrap_or_else(|| "localhost".to_string())
    }

    fn save_cookies_if_configured(&self) {
        if let Some(ref cf) = self.cookie_file {
            if let Err(e) = self.cookie_storage.save_file(std::path::Path::new(cf)) {
                warn!("保存 Cookie 文件失败 {}: {}", cf, e);
            } else {
                info!("Cookie 已保存到 {}", cf);
            }
        }
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }

    pub async fn group_mut(&self) -> tokio::sync::RwLockWriteGuard<'_, RequestGroup> {
        self.group.write().await
    }

    fn should_use_concurrent(&self, total_length: u64, supports_range: bool) -> bool {
        if !supports_range {
            return false;
        }
        if total_length < CONCURRENT_MIN_FILE_SIZE {
            return false;
        }

        let split = { self.group.blocking_read().options().split.unwrap_or(1) };
        split > 1
    }

    async fn execute_sequential_download(
        &mut self,
        uri: &str,
        resume_state: &crate::filesystem::resume_helper::ResumeState,
        total_length: u64,
    ) -> Result<()> {
        let url_parsed = reqwest::Url::parse(uri).ok();
        let mut request = if let Some(range_header) = ResumeHelper::build_range_header(resume_state)
        {
            debug!("断点续传: {}", range_header);
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

        let response = request.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP请求失败: {}", e),
            })
        })?;

        if let Some(ref url) = url_parsed {
            let domain = url.host_str().unwrap_or("");
            let path = url.path();
            for sc_val in response.headers().get_all("set-cookie").iter() {
                if let Ok(sc_str) = sc_val.to_str() {
                    if let Some(c) = Cookie::from_set_cookie_header(sc_str, domain, path) {
                        self.cookie_storage.add(c);
                        debug!("收到 Set-Cookie: {}", sc_str);
                    }
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
                format!("HTTP错误: {}", status),
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
                debug!("下载限速启用: {} bytes/s", rate);
                Box::new(ThrottledWriter::new(raw_writer, limiter))
            }
            _ => Box::new(raw_writer),
        };

        let mut stream = response.bytes_stream();
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        while let Some(chunk) = stream.next().await {
            let data: bytes::Bytes = chunk.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
            writer.write(&data).await?;
            self.completed_bytes += data.len() as u64;

            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;
                // Export to atomic fields for session persistence
                g.set_completed_length(self.completed_bytes);
                g.set_download_speed_cached(0); // Will be updated below

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    // Update cached download speed for session persistence
                    g.set_download_speed_cached(speed);
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        writer.finalize().await.ok();

        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
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
            "顺序下载完成: {} ({} bytes)",
            self.output_path.display(),
            self.completed_bytes
        );
        self.save_cookies_if_configured();
        Ok(())
    }

    async fn execute_concurrent_download(&mut self, uri: &str, total_length: u64) -> Result<()> {
        let options = self.group.read().await.options().clone();
        let split = options.split.unwrap_or(1) as usize;
        let max_conn = options.max_connection_per_server.unwrap_or(4) as usize;
        let seg_size = total_length / split as u64;

        info!(
            "并发下载启动: split={}, max_conn={}, segment_size={} bytes, total={}",
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

        let start_time = Instant::now();
        let mut active_handles: std::collections::HashMap<
            u32,
            tokio::task::JoinHandle<std::result::Result<Vec<u8>, String>>,
        > = std::collections::HashMap::new();

        loop {
            while active_handles.len() < max_conn {
                match manager.next_pending_segment_for_mirror(0) {
                    Some((seg_idx, offset, length)) => {
                        let url = uri.to_string();
                        let dl = HttpSegmentDownloader::new(&self.client);
                        let ch = cookie_hdr_for_spawn.clone();
                        let handle = tokio::spawn(async move {
                            dl.download_range(&url, offset, length, ch.as_deref())
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
                                            format!("写入失败: {}", e),
                                        ))
                                    })?;
                                } else {
                                    writer.write_at(0, &data).await.ok();
                                }
                                manager.complete_segment(seg_idx, data.clone());
                                self.completed_bytes += data.len() as u64;

                                let mut g = self.group.write().await;
                                g.update_progress(self.completed_bytes).await;
                                // Export to atomic fields for session persistence
                                g.set_completed_length(self.completed_bytes);
                            }
                            Ok(Err(e)) => {
                                warn!("段 {} 下载失败: {}", seg_idx, e);
                                manager.fail_segment(seg_idx);
                            }
                            Err(_) => {
                                warn!("段 {} 任务 panic", seg_idx);
                                manager.fail_segment(seg_idx);
                            }
                        }
                    }
                }
            }

            if manager.is_complete() {
                debug!("所有段已完成");
                break;
            }

            if manager.has_failed_segments()
                && !manager.has_pending_segments()
                && active_handles.is_empty()
            {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "并发下载所有段均失败".into(),
                    },
                ));
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        writer.flush().await.map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "flush 失败: {}",
                e
            )))
        })?;

        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
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
            "并发下载完成: {} ({} bytes)",
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
            if attempt > 0 {
                if let Some(wait) = retry_policy.compute_wait(attempt - 1) {
                    info!("顺序下载重试 #{} (等待 {:?})...", attempt, wait);
                    tokio::time::sleep(wait).await;
                }
            }

            match self
                .execute_sequential_download(uri, resume_state, _total_length)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("顺序下载尝试 #{} 失败: {}", attempt + 1, e);
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
                message: "所有重试均失败".into(),
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
            "使用并发模式下载 (split={}, max_retries/segment={})",
            self.group.read().await.options().split.unwrap_or(1),
            max_retries_per_segment
        );

        let split = self.group.read().await.options().split.unwrap_or(1) as u64;
        let segment_size = (total_length + split - 1) / split;
        let mut manager =
            ConcurrentSegmentManager::new(total_length, vec![uri.to_string()], Some(segment_size));
        manager.set_max_retries(max_retries_per_segment);

        if resume_state.should_resume {
            manager.mark_completed_up_to(resume_state.start_offset, resume_state.existing_length);
        }

        let mut writer = CachedDiskWriter::new(&self.output_path, Some(total_length), None);

        while manager.has_pending_segments() || !manager.is_complete() {
            while let Some((seg_idx, offset, length)) = manager.next_pending_segment() {
                let seg_idx_u32 = seg_idx as u32;
                info!(
                    "启动段 {} 下载: offset={}, size={}",
                    seg_idx, offset, length
                );
                let downloader = HttpSegmentDownloader::new(&self.client);
                let result = downloader.download_range(uri, offset, length, None).await;

                match result {
                    Ok(data) => {
                        debug!("段 {} 完成: {} bytes", seg_idx, data.len());
                        writer.write_at(offset, &data).await.map_err(|e| {
                            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                                "写入失败: {}",
                                e
                            )))
                        })?;
                        manager.complete_segment(seg_idx_u32, data);
                    }
                    Err(e) => {
                        warn!("段 {} 下载失败: {}", seg_idx, e);
                        manager.fail_segment(seg_idx_u32);
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
                error!("存在永久失败的下载段");
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "部分下载段永久失败".into(),
                    },
                ));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        writer.flush().await.map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "flush 失败: {}",
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
                "下载URI为空".into(),
            )));
        }

        debug!("开始下载: {} -> {}", uri, self.output_path.display());

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "创建目录失败: {}",
                        e
                    )))
                })?;
            }
        }

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
        let head_resp = head_req.send().await.ok();
        let (total_length, supports_range) = if let Some(ref resp) = head_resp {
            let tl = resp.content_length().unwrap_or(0);
            let sr = resp
                .headers()
                .get("Accept-Ranges")
                .and_then(|v| v.to_str().ok())
                .map_or(false, |v| v.to_lowercase().contains("bytes"));
            (tl, sr)
        } else {
            (0, false)
        };

        let resume_helper = ResumeHelper::new(&self.output_path, self.continue_enabled);
        let resume_state = resume_helper.detect(total_length).await?;

        if resume_state.is_complete {
            info!(
                "文件已完整存在，跳过下载: {} ({} bytes)",
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
                    "并发模式 + 断点续传: 已有 {} bytes, 从偏移 {} 继续",
                    resume_state.existing_length, resume_state.start_offset
                );
            }
            let max_retries = options.max_retries;
            return self
                .execute_concurrent_download_with_retry(
                    &uri,
                    total_length,
                    &resume_state,
                    max_retries,
                )
                .await;
        }

        let retry_policy = RetryPolicy::new(options.max_retries, options.retry_wait * 1000);
        self.execute_sequential_download_with_retry(
            &uri,
            &resume_state,
            total_length,
            &retry_policy,
        )
        .await
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(300))
    }
}
