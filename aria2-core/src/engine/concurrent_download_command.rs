use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::control_file;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::file_allocation;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

pub struct ConcurrentDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    client: reqwest::Client,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    metalink_data: Vec<u8>,
    max_connections_per_server: u16,
    file_allocation: String,
    disk_cache_size_mb: Option<usize>,
}

impl ConcurrentDownloadCommand {
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

        let file = doc
            .single_file()
            .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("No file in Metalink".into())))?;

        if file.urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "No URLs in Metalink".into(),
            )));
        }

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let path = std::path::PathBuf::from(&dir).join(&file.name);
        let urls: Vec<String> = file
            .get_sorted_urls()
            .iter()
            .map(|u| u.url.clone())
            .collect();
        let group = RequestGroup::new(gid, urls.clone(), options.clone());
        let max_conn = options.max_connection_per_server.unwrap_or(2);

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

        let alloc = "prealloc".to_string();
        let cache_mb: Option<usize> = None;

        info!(
            "ConcurrentDownloadCommand created: {} -> {} ({} mirrors)",
            file.name,
            path.display(),
            urls.len()
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            client,
            output_path: path,
            started: false,
            completed_bytes: 0,
            metalink_data: metalink_bytes.to_vec(),
            max_connections_per_server: max_conn,
            file_allocation: alloc,
            disk_cache_size_mb: cache_mb,
        })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }
}

#[async_trait]
impl Command for ConcurrentDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(&self.metalink_data)
            .map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("Metalink parse error: {}", e)))
            })?;

        let file = doc
            .single_file()
            .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("No available file".into())))?;

        let sorted_urls = file.get_sorted_urls();
        if sorted_urls.len() < 2 {
            warn!(
                "Concurrent download requires 2+ mirrors, got {}. Falling back to sequential.",
                sorted_urls.len()
            );
        }

        let urls: Vec<String> = sorted_urls.iter().map(|u| u.url.clone()).collect();
        let expected_size = file.size;
        let hash_entry = file.hashes.first().cloned();

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
                })?;
            }
        }

        let mut manager = ConcurrentSegmentManager::new(expected_size.unwrap_or(0), urls, None);
        manager.set_max_connections_per_mirror(self.max_connections_per_server as usize);

        if manager.num_segments() == 0 && expected_size != Some(0) {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Cannot determine file size for segmentation".into(),
            )));
        }
        if expected_size == Some(0) || manager.num_segments() == 0 {
            return Ok(());
        }

        let total_len = expected_size.unwrap_or(0);
        if total_len > 0 {
            file_allocation::preallocate_file(&self.output_path, total_len, &self.file_allocation)
                .await?;
        }

        let num_pieces = manager.num_segments().max(1);
        let ctrl_path = control_file::ControlFile::control_path_for(&self.output_path);
        let mut ctrl_file =
            control_file::ControlFile::open_or_create(&ctrl_path, total_len, num_pieces).await?;
        ctrl_file.save().await.ok();

        let mut writer =
            CachedDiskWriter::new(&self.output_path, expected_size, self.disk_cache_size_mb);

        let rate_limit = {
            let g = self.group.read().await;
            g.options().max_download_limit
        };
        let limiter = rate_limit.filter(|&r| r > 0).map(|r| {
            use crate::rate_limiter::RateLimiterConfig;
            RateLimiter::new(&RateLimiterConfig::new(Some(r), None))
        });

        Self::download_concurrent_to_disk(
            &self.client,
            &mut manager,
            &mut writer,
            &mut ctrl_file,
            limiter.as_ref(),
        )
        .await
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Concurrent download failed: {}", e),
            })
        })?;

        writer
            .flush()
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        if let Some(ref hash) = hash_entry {
            let file_data = writer
                .read_all()
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
            if !Self::verify_hash(&file_data, hash)? {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Hash verification failed after concurrent download".into(),
                    },
                ));
            }
        }

        writer
            .close()
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        self.completed_bytes = total_len;

        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(self.completed_bytes, 0).await;
            g.complete().await?;
        }

        info!(
            "Concurrent download done: {} ({} bytes from {} mirrors)",
            self.output_path.display(),
            self.completed_bytes,
            manager.num_mirrors()
        );
        Ok(())
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

impl ConcurrentDownloadCommand {
    async fn download_concurrent_to_disk(
        client: &reqwest::Client,
        manager: &mut ConcurrentSegmentManager,
        writer: &mut CachedDiskWriter,
        ctrl_file: &mut control_file::ControlFile,
        limiter: Option<&RateLimiter>,
    ) -> std::result::Result<(), String> {
        manager.allocate_segments();

        if manager.num_segments() <= 1 {
            return Self::download_single_mirror_fallback_to_disk(client, manager, writer, limiter)
                .await;
        }

        let mut iteration = 0u32;
        loop {
            let mut handles = Vec::new();

            for mi in 0..manager.num_mirrors() {
                while let Some((seg_idx, offset, length)) =
                    manager.next_pending_segment_for_mirror(mi)
                {
                    let url = manager.mirror_url(mi).unwrap_or("").to_string();
                    let client_clone = client.clone();
                    let seg_idx_copy = seg_idx;

                    let handle = tokio::spawn(async move {
                        Self::download_single_segment(
                            &client_clone,
                            &url,
                            offset,
                            length,
                            seg_idx_copy,
                        )
                        .await
                    });
                    handles.push((mi, seg_idx, offset, handle));
                }
            }

            if handles.is_empty() {
                if manager.is_complete() {
                    break;
                }
                if manager.has_failed_segments() {
                    return Err("Some segments exhausted max retries".to_string());
                }
                if !manager.any_mirror_available() && manager.has_pending_segments() {
                    return Err("All mirrors unavailable with pending segments".to_string());
                }
                iteration += 1;
                if iteration > 100 {
                    return Err("Download timed out in concurrent loop".to_string());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            for (mi, seg_idx, offset, handle) in handles {
                match handle.await {
                    Ok(Ok(data)) => {
                        writer
                            .write_at(offset, &data)
                            .await
                            .map_err(|e| format!("Disk write error seg{}: {}", seg_idx, e))?;
                        manager.complete_segment(seg_idx, data);
                        ctrl_file.mark_piece_done(seg_idx as usize);
                        ctrl_file.save().await.ok();
                    }
                    Ok(Err(e)) => {
                        warn!("Segment {} from mirror {} failed: {}", seg_idx, mi, e);
                        manager.fail_segment(seg_idx);
                    }
                    Err(join_err) => {
                        warn!("Segment {} task panicked: {}", seg_idx, join_err);
                        manager.fail_segment(seg_idx);
                    }
                }
            }

            if manager.is_complete() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Ok(())
    }

    async fn download_single_segment(
        client: &reqwest::Client,
        url: &str,
        offset: u64,
        length: u64,
        _seg_idx: u32,
    ) -> std::result::Result<Vec<u8>, String> {
        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));

        let response = client
            .get(url)
            .header("Range", &range_header)
            .send()
            .await
            .map_err(|e| format!("HTTP Range request failed: {}", e))?;

        let status = response.status();
        if status.as_u16() >= 400 {
            return Err(format!("HTTP error on segment: {}", status));
        }

        let mut data = Vec::with_capacity(length as usize);
        let mut stream = response.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => data.extend_from_slice(&bytes),
                Err(e) => return Err(format!("Stream read error: {}", e)),
            }
        }

        if data.is_empty() && length > 0 {
            return Err("Empty segment data".to_string());
        }

        Ok(data)
    }

    fn verify_hash(
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

    async fn download_single_mirror_fallback_to_disk(
        client: &reqwest::Client,
        manager: &mut ConcurrentSegmentManager,
        writer: &mut CachedDiskWriter,
        limiter: Option<&RateLimiter>,
    ) -> std::result::Result<(), String> {
        for mi in 0..manager.num_mirrors() {
            let url = match manager.mirror_url(mi) {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => continue,
            };

            match client.get(&url).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() || status.as_u16() == 206 {
                        let mut offset: u64 = 0;
                        let mut stream = response.bytes_stream();
                        while let Some(chunk_result) = stream.next().await {
                            match chunk_result {
                                Ok(bytes) => {
                                    if let Some(lim) = limiter {
                                        lim.acquire_download(bytes.len() as u64).await;
                                    }
                                    writer
                                        .write_at(offset, &bytes)
                                        .await
                                        .map_err(|e| format!("Write error: {}", e))?;
                                    offset += bytes.len() as u64;
                                }
                                Err(e) => return Err(format!("Stream read error: {}", e)),
                            }
                        }

                        manager.complete_segment(0, vec![]);
                        return Ok(());
                    }
                }
                Err(_) => continue,
            }
        }
        Err("All mirrors failed for single-segment download".to_string())
    }
}
