use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

pub struct MetalinkDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    client: reqwest::Client,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    metalink_data: Vec<u8>,
}

impl MetalinkDownloadCommand {
    pub fn new(
        gid: GroupId,
        metalink_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(metalink_bytes)
            .map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("Metalink parse failed: {}", e)))
            })?;

        let file = doc.single_file().ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config(
                "Metalink contains multiple files or no files".into(),
            ))
        })?;

        if file.urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Metalink file has no download URLs".into(),
            )));
        }

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = file.name.clone();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        let urls: Vec<String> = file
            .get_sorted_urls()
            .iter()
            .map(|u| u.url.clone())
            .collect();
        let group = RequestGroup::new(gid, urls, options.clone());

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .user_agent("aria2-rust/0.1.0")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "HTTP client build failed: {}",
                    e
                )))
            })?;

        info!(
            "MetalinkDownloadCommand created: {} -> {} ({} mirrors)",
            file.name,
            path.display(),
            file.urls.len()
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            client,
            output_path: path,
            started: false,
            completed_bytes: 0,
            metalink_data: metalink_bytes.to_vec(),
        })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }
}

#[async_trait]
impl Command for MetalinkDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(&self.metalink_data)
            .map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("Metalink parse error: {}", e)))
            })?;

        let file = doc.single_file().ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config("No available file after parsing".into()))
        })?;

        let sorted_urls = file.get_sorted_urls();
        if sorted_urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "No download mirrors available".into(),
            )));
        }

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
                })?;
            }
        }

        let expected_size = file.size;
        let hash_entry = file.hashes.first().cloned();

        let mut last_error = None;

        for url_entry in &sorted_urls {
            debug!(
                "Trying mirror [priority={}] : {}",
                url_entry.priority, url_entry.url
            );

            match self.try_download_url(&url_entry.url, expected_size).await {
                Ok(data) => {
                    if let Some(ref hash) = hash_entry {
                        if !self.verify_hash(&data, hash)? {
                            warn!(
                                "Hash verification failed [{}]: trying next mirror",
                                hash.algo.as_standard_name()
                            );
                            last_error = Some(Aria2Error::Recoverable(
                                RecoverableError::TemporaryNetworkFailure {
                                    message: format!(
                                        "Hash verification failed: {}",
                                        hash.algo.as_standard_name()
                                    ),
                                },
                            ));
                            continue;
                        }
                    }

                    let raw_writer = DefaultDiskWriter::new(&self.output_path);
                    let rate_limit = {
                        let g = self.group.read().await;
                        g.options().max_download_limit
                    };
                    let mut writer: Box<dyn DiskWriter> = match rate_limit {
                        Some(rate) if rate > 0 => Box::new(ThrottledWriter::new(
                            raw_writer,
                            RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)),
                        )),
                        _ => Box::new(raw_writer),
                    };
                    writer.write(&data).await?;
                    writer.finalize().await.ok();

                    self.completed_bytes = data.len() as u64;

                    {
                        let mut g = self.group.write().await;
                        g.update_progress(self.completed_bytes).await;
                        g.update_speed(self.completed_bytes, 0).await;
                        g.complete().await?;
                    }

                    info!(
                        "Metalink download done: {} ({} bytes from {})",
                        self.output_path.display(),
                        self.completed_bytes,
                        url_entry.url
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!("Mirror download failed {}: {}", url_entry.url, e);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| Aria2Error::Fatal(FatalError::Config("All mirrors failed".into()))))
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }
}

impl MetalinkDownloadCommand {
    async fn try_download_url(&mut self, url: &str, expected_size: Option<u64>) -> Result<Vec<u8>> {
        let response = self.client.get(url).send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP request failed: {}", e),
            })
        })?;

        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            if status.as_u16() >= 500 {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code: status.as_u16(),
                }));
            }
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "HTTP error: {}",
                status
            ))));
        }

        let total_length = response.content_length().unwrap_or(0) as u64;

        {
            let mut g = self.group.write().await;
            g.set_total_length(total_length.max(expected_size.unwrap_or(0)))
                .await;
        }

        let mut data = Vec::with_capacity(total_length as usize);
        let mut stream = response.bytes_stream();
        let _start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        while let Some(chunk_result) = stream.next().await {
            let bytes: bytes::Bytes = chunk_result.map_err(|e: reqwest::Error| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
            data.extend_from_slice(&bytes);
            self.completed_bytes = data.len() as u64;

            let elapsed = last_speed_update.elapsed();
            if elapsed.as_millis() >= 500 {
                let delta = self.completed_bytes - last_completed;
                let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;
                g.update_speed(speed, 0).await;
                last_speed_update = Instant::now();
                last_completed = self.completed_bytes;
            }
        }

        Ok(data)
    }

    fn verify_hash(
        &self,
        data: &[u8],
        hash: &aria2_protocol::metalink::parser::HashEntry,
    ) -> Result<bool> {
        use aria2_protocol::metalink::parser::HashAlgorithm;

        match hash.algo {
            HashAlgorithm::Md5 => {
                let digest = md5::compute(data);
                Ok(format!("{:x}", digest) == hash.value)
            }
            HashAlgorithm::Sha1 => {
                use sha1::Digest;
                let mut hasher = sha1::Sha1::new();
                hasher.update(data);
                let result = hasher.finalize();
                Ok(format!("{:x}", result) == hash.value)
            }
            HashAlgorithm::Sha256 => {
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                hasher.update(data);
                let result = hasher.finalize();
                Ok(format!("{:x}", result) == hash.value)
            }
            HashAlgorithm::Sha512 => {
                use sha2::Digest;
                let mut hasher = sha2::Sha512::new();
                hasher.update(data);
                let result = hasher.finalize();
                Ok(format!("{:x}", result) == hash.value)
            }
        }
    }
}
