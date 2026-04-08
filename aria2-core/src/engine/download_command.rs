use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use tracing::{info, debug, warn};
use futures::StreamExt;

use crate::error::{Aria2Error, Result, RecoverableError};
use crate::engine::command::{Command, CommandStatus};
use crate::engine::http_segment_downloader::HttpSegmentDownloader;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::request::request_group::{RequestGroup, GroupId, DownloadOptions};
use crate::filesystem::disk_writer::{DiskWriter, DefaultDiskWriter, CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::file_allocation;
use crate::filesystem::resume_helper::ResumeHelper;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};

const CONCURRENT_MIN_FILE_SIZE: u64 = 1024 * 1024;

pub struct DownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    client: reqwest::Client,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    continue_enabled: bool,
    file_allocation: String,
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

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .user_agent("aria2-rust/0.1.0")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| Aria2Error::Fatal(crate::error::FatalError::Config(format!("创建HTTP客户端失败: {}", e))))?;

        let group = RequestGroup::new(gid, vec![uri.to_string()], options.clone());
        info!("DownloadCommand 创建: {} -> {}", uri, path.display());

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            client,
            output_path: path,
            started: false,
            completed_bytes: 0,
            continue_enabled: true,
            file_allocation: "prealloc".to_string(),
        })
    }

    fn extract_filename(uri: &str) -> Option<String> {
        uri.rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "/")
            .map(|s| {
                s.split('?').next().unwrap_or(s).to_string()
            })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }

    pub async fn group_mut(&self) -> tokio::sync::RwLockWriteGuard<'_, RequestGroup> {
        self.group.write().await
    }

    fn should_use_concurrent(&self, total_length: u64, supports_range: bool) -> bool {
        if !supports_range { return false; }
        if total_length < CONCURRENT_MIN_FILE_SIZE { return false; }

        let split = { self.group.blocking_read().options().split.unwrap_or(1) };
        split > 1
    }

    async fn execute_sequential_download(
        &mut self,
        uri: &str,
        resume_state: &crate::filesystem::resume_helper::ResumeState,
        total_length: u64,
    ) -> Result<()> {
        let request = if let Some(range_header) = ResumeHelper::build_range_header(resume_state) {
            debug!("断点续传: {}", range_header);
            self.client.get(uri).header("Range", range_header)
        } else {
            self.client.get(uri)
        };

        let response = request.send().await
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: format!("HTTP请求失败: {}", e) }))?;

        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            if status.as_u16() >= 500 {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError { code: status.as_u16() }));
            }
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(format!("HTTP错误: {}", status))));
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
        }

        let start_offset = if resume_state.should_resume { resume_state.start_offset } else { 0 };
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
            let data: bytes::Bytes = chunk.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e.to_string() }))?;
            writer.write(&data).await?;
            self.completed_bytes += data.len() as u64;

            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        writer.finalize().await.ok();

        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 { (self.completed_bytes as f64 / elapsed) as u64 } else { 0 }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.complete().await?;
        }

        info!("顺序下载完成: {} ({} bytes)", self.output_path.display(), self.completed_bytes);
        Ok(())
    }

    async fn execute_concurrent_download(
        &mut self,
        uri: &str,
        total_length: u64,
    ) -> Result<()> {
        let options = self.group.read().await.options().clone();
        let split = options.split.unwrap_or(1) as usize;
        let max_conn = options.max_connection_per_server.unwrap_or(4) as usize;
        let seg_size = total_length / split as u64;

        info!("并发下载启动: split={}, max_conn={}, segment_size={} bytes, total={}", split, max_conn, seg_size, total_length);

        let mut manager = ConcurrentSegmentManager::new(total_length, vec![uri.to_string()], Some(seg_size));
        manager.set_max_connections_per_mirror(max_conn.min(split));
        manager.allocate_segments();

        let mut writer = CachedDiskWriter::new(&self.output_path, Some(total_length), None);

        let limiter = options.max_download_limit.filter(|&r| r > 0).map(|r| {
            RateLimiter::new(&RateLimiterConfig::new(Some(r), None))
        });

        let start_time = Instant::now();
        let mut active_handles: std::collections::HashMap<u32, tokio::task::JoinHandle<std::result::Result<Vec<u8>, String>>> = std::collections::HashMap::new();

        loop {
            while active_handles.len() < max_conn {
                match manager.next_pending_segment_for_mirror(0) {
                    Some((seg_idx, offset, length)) => {
                        let url = uri.to_string();
                        let dl = HttpSegmentDownloader::new(&self.client);
                        let handle = tokio::spawn(async move {
                            dl.download_range(&url, offset, length).await
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
                                if let Some((offset, _, _)) = manager.segment_info(seg_idx as usize) {
                                    writer.write_at(offset, &data).await
                                        .map_err(|e| Aria2Error::Fatal(crate::error::FatalError::Config(format!("写入失败: {}", e))))?;
                                } else {
                                    writer.write_at(0, &data).await.ok();
                                }
                                manager.complete_segment(seg_idx, data.clone());
                                self.completed_bytes += data.len() as u64;

                                let mut g = self.group.write().await;
                                g.update_progress(self.completed_bytes).await;
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

            if manager.has_failed_segments() && !manager.has_pending_segments() && active_handles.is_empty() {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure { message: "并发下载所有段均失败".into() }
                ));
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        writer.flush().await.map_err(|e| Aria2Error::Fatal(crate::error::FatalError::Config(format!("flush 失败: {}", e))))?;

        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 { (self.completed_bytes as f64 / elapsed) as u64 } else { 0 }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.complete().await?;
        }

        info!("并发下载完成: {} ({} bytes)", self.output_path.display(), self.completed_bytes);
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
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config("下载URI为空".into())));
        }

        debug!("开始下载: {} -> {}", uri, self.output_path.display());

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Aria2Error::Fatal(crate::error::FatalError::Config(format!("创建目录失败: {}", e))))?;
            }
        }

        let head_resp = self.client.head(&uri).send().await.ok();
        let (total_length, supports_range) = if let Some(ref resp) = head_resp {
            let tl = resp.content_length().unwrap_or(0);
            let sr = resp.headers().get("Accept-Ranges").and_then(|v| v.to_str().ok()).map_or(false, |v| v.to_lowercase().contains("bytes"));
            (tl, sr)
        } else {
            (0, false)
        };

        let resume_helper = ResumeHelper::new(&self.output_path, self.continue_enabled);
        let resume_state = resume_helper.detect(total_length).await?;

        if resume_state.is_complete {
            info!("文件已完整存在，跳过下载: {} ({} bytes)", self.output_path.display(), resume_state.existing_length);
            self.completed_bytes = resume_state.existing_length;
            let mut g = self.group.write().await;
            g.set_total_length(self.completed_bytes).await;
            g.update_progress(self.completed_bytes).await;
            g.complete().await?;
            return Ok(());
        }

        if total_length > 0 {
            file_allocation::preallocate_file(&self.output_path, total_length, &self.file_allocation).await?;
        }

        if self.should_use_concurrent(total_length, supports_range) && !resume_state.should_resume {
            info!("使用并发模式下载 (split={}, supports_range={})", self.group.blocking_read().options().split.unwrap_or(1), supports_range);
            return self.execute_concurrent_download(&uri, total_length).await;
        }

        self.execute_sequential_download(&uri, &resume_state, total_length).await
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 { CommandStatus::Running } else { CommandStatus::Pending }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(300))
    }
}
