use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use tracing::{info, debug, warn};
use futures::StreamExt;

use crate::error::{Aria2Error, Result, RecoverableError};
use crate::engine::command::{Command, CommandStatus};
use crate::request::request_group::{RequestGroup, GroupId, DownloadOptions};
use crate::filesystem::disk_writer::{DiskWriter, DefaultDiskWriter, CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::file_allocation;
use crate::filesystem::resume_helper::ResumeHelper;

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
        let total_length = head_resp.and_then(|r| r.content_length()).unwrap_or(0);

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

        let request = if let Some(range_header) = ResumeHelper::build_range_header(&resume_state) {
            debug!("断点续传: {}", range_header);
            self.client.get(&uri).header("Range", range_header)
        } else {
            self.client.get(&uri)
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
        let mut writer: Box<dyn DiskWriter> = if resume_state.should_resume {
            info!("续传模式: 从偏移 {} 开始", start_offset);
            Box::new(DefaultDiskWriter::new(&self.output_path))
        } else {
            Box::new(DefaultDiskWriter::new(&self.output_path))
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

        info!("下载完成: {} ({} bytes)", self.output_path.display(), self.completed_bytes);
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 { CommandStatus::Running } else { CommandStatus::Pending }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(300))
    }
}
