use async_trait::async_trait;
use futures::StreamExt;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::active_output_registry::global_registry;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use super::MetalinkDownloadCommand;

#[async_trait]
impl Command for MetalinkDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        // Resolve file info: either from pre-parsed file_info (multi-file mode)
        // or by re-parsing the raw metalink_data (single-file mode).
        // We extract owned data to avoid lifetime/borrow issues.
        let sorted_urls_owned: Vec<aria2_protocol::metalink::parser::UrlEntry>;
        let expected_size: Option<u64>;
        let hash_entry_owned: Option<aria2_protocol::metalink::parser::HashEntry>;

        match &self.file_info {
            Some(info) => {
                sorted_urls_owned = info.sorted_urls.clone();
                expected_size = info.expected_size;
                hash_entry_owned = info.hash_entry.clone();
            }
            None => {
                let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(
                    &self.metalink_data,
                    None,
                )
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("Metalink parse error: {}", e)))
                })?;

                let file = if doc.files.len() == 1 {
                    &doc.files[0]
                } else {
                    // Multi-file Metalink in single-file mode: use first file
                    &doc.files[0]
                };

                sorted_urls_owned = file
                    .get_sorted_urls()
                    .iter()
                    .map(|u| (*u).clone())
                    .collect();
                expected_size = file.size;
                hash_entry_owned = file.hashes.first().cloned();

                if sorted_urls_owned.is_empty() {
                    return Err(Aria2Error::Fatal(FatalError::Config(
                        "No download mirrors available".into(),
                    )));
                }
            }
        }

        if sorted_urls_owned.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "No download mirrors available".into(),
            )));
        }

        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        // Resolve filename collision against other active downloads.
        // If another task is already writing to self.output_path, a unique
        // name such as "file (1).ext" will be generated automatically.
        let resolved_output_path = global_registry().resolve(&self.output_path).await;

        // Helper closure to release the resolved path on every exit path.
        let release_path = |path: &std::path::Path| {
            let p = path.to_path_buf();
            // Best-effort async release; safe to drop the spawned future.
            #[allow(clippy::let_underscore_future)]
            let _ = tokio::spawn(async move {
                global_registry().release(&p).await;
            });
        };

        let mut last_error = None;

        for url_entry in &sorted_urls_owned {
            debug!(
                "Trying mirror [priority={}] : {}",
                url_entry.priority, url_entry.url
            );

            match self.try_download_url(&url_entry.url, expected_size).await {
                Ok(data) => {
                    if let Some(ref hash) = hash_entry_owned
                        && !self.verify_hash(&data, hash)?
                    {
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

                    let raw_writer = DefaultDiskWriter::new(&resolved_output_path);
                    let rate_limit = {
                        let g = self.group.recover();
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
                        let g = self.group.recover();
                        g.update_progress(self.completed_bytes);
                        g.update_speed(self.completed_bytes, 0);
                        drop(g);
                        let mut g = self.group.recover_mut();
                        g.complete()?;
                    }

                    info!(
                        "Metalink download done: {} ({} bytes from {})",
                        resolved_output_path.display(),
                        self.completed_bytes,
                        url_entry.url
                    );
                    self.completed = true;
                    release_path(&resolved_output_path);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Mirror download failed {}: {}", url_entry.url, e);
                    last_error = Some(e);
                }
            }
        }

        release_path(&resolved_output_path);
        Err(last_error
            .unwrap_or_else(|| Aria2Error::Fatal(FatalError::Config("All mirrors failed".into()))))
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

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }
}

impl MetalinkDownloadCommand {
    pub(crate) async fn try_download_url(
        &mut self,
        url: &str,
        expected_size: Option<u64>,
    ) -> Result<Vec<u8>> {
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

        // Read Content-Length from the header directly instead of using
        // response.content_length(), which returns the *body* size. For chunked
        // transfer encoding or proxy-modified responses the body size may differ
        // from the advertised header value. The header value is what the server
        // advertised and is consistent with download_command.rs's approach.
        let total_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        {
            let g = self.group.recover();
            g.set_total_length(total_length.max(expected_size.unwrap_or(0)));
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
                let g = self.group.recover();
                g.update_progress(self.completed_bytes);
                g.update_speed(speed, 0);
                last_speed_update = Instant::now();
                last_completed = self.completed_bytes;
            }
        }

        Ok(data)
    }

    pub(crate) fn verify_hash(
        &self,
        data: &[u8],
        hash: &aria2_protocol::metalink::parser::HashEntry,
    ) -> Result<bool> {
        use aria2_protocol::metalink::parser::HashAlgorithm;

        match hash.algo {
            HashAlgorithm::Md5 => {
                use md5::Digest;
                let mut hasher = md5::Md5::new();
                hasher.update(data);
                let digest = hasher.finalize();
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
