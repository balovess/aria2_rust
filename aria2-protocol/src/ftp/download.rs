use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::connection::{FtpActiveDataListener, FtpConnection, FtpResponseClass};
use super::listing::parse_ftp_list_response;

mod types;
pub use types::{DownloadProgress, DownloadResult, FtpDownloadOptions};

#[cfg(test)]
mod tests;

/// FTP download manager that handles file transfers
pub struct FtpDownload<'a> {
    conn: &'a mut FtpConnection,
    options: FtpDownloadOptions,
}

enum FtpDataConnection {
    Passive { host: String, port: u16 },
    Active(FtpActiveDataListener),
}

impl FtpDataConnection {
    async fn open(self, timeout_duration: Duration) -> Result<TcpStream, String> {
        match self {
            Self::Passive { host, port } => {
                match timeout(timeout_duration, TcpStream::connect((host.as_str(), port))).await {
                    Ok(result) => result.map_err(|e| format!("FTP data connection failed: {}", e)),
                    Err(_) => Err(format!(
                        "FTP data connection timeout ({}s)",
                        timeout_duration.as_secs()
                    )),
                }
            }
            Self::Active(listener) => match timeout(timeout_duration, listener.accept()).await {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "FTP active data connection timeout ({}s)",
                    timeout_duration.as_secs()
                )),
            },
        }
    }
}

/// Check if an IO error is transient and retry-worthy.
fn is_transient_io_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    matches!(
        error.kind(),
        ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
    ) || error.to_string().to_lowercase().contains("temporary")
}

impl<'a> FtpDownload<'a> {
    /// Create a new FTP download manager
    pub fn new(conn: &'a mut FtpConnection, options: Option<FtpDownloadOptions>) -> Self {
        Self {
            conn,
            options: options.unwrap_or_default(),
        }
    }

    /// Download a single file from FTP server to local filesystem
    ///
    /// # Arguments
    /// * `remote_path` - Path to the file on the FTP server
    /// * `local_path` - Local path where the file should be saved
    /// * `progress_callback` - Optional callback for progress updates
    pub async fn download_file(
        &mut self,
        remote_path: &str,
        local_path: &str,
        progress_callback: Option<fn(DownloadProgress)>,
    ) -> Result<DownloadResult, String> {
        self.download_file_controlled(remote_path, local_path, progress_callback, None)
            .await
    }

    /// Download a file while observing a cancellation token.
    ///
    /// Cancellation is checked between data reads. The data connection is
    /// closed and FTP `ABOR` is sent before returning the cancellation error.
    pub async fn download_file_with_cancellation(
        &mut self,
        remote_path: &str,
        local_path: &str,
        progress_callback: Option<fn(DownloadProgress)>,
        cancellation: &CancellationToken,
    ) -> Result<DownloadResult, String> {
        self.download_file_controlled(
            remote_path,
            local_path,
            progress_callback,
            Some(cancellation),
        )
        .await
    }

    async fn download_file_controlled(
        &mut self,
        remote_path: &str,
        local_path: &str,
        progress_callback: Option<fn(DownloadProgress)>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DownloadResult, String> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("FTP download cancelled".to_string());
        }

        // Set transfer mode (binary by default)
        if self.options.binary_mode {
            self.conn.type_image().await?;
        } else {
            self.conn.type_ascii().await?;
        }

        // Probe file size before download
        let file_size = self.conn.size(remote_path).await.ok();

        // Set resume offset if resuming
        if let Some(offset) = self.options.resume_offset
            && offset > 0
        {
            self.conn.rest(offset).await?;
        }

        // Establish data connection (try passive mode first)
        let data_connection = self.establish_data_connection().await?;

        // Send RETR command to initiate transfer
        self.conn.retr(remote_path).await?;

        // Connect to data port and receive file content
        let result = self
            .receive_data_to_file(
                data_connection,
                local_path,
                file_size,
                progress_callback,
                cancellation,
            )
            .await?;

        Ok(result)
    }

    /// Download a file into memory (for small files or when disk I/O is not needed)
    pub async fn download_to_memory(&mut self, remote_path: &str) -> Result<Vec<u8>, String> {
        self.download_to_memory_controlled(remote_path, None).await
    }

    /// Download a file into memory while observing a cancellation token.
    pub async fn download_to_memory_with_cancellation(
        &mut self,
        remote_path: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, String> {
        self.download_to_memory_controlled(remote_path, Some(cancellation))
            .await
    }

    async fn download_to_memory_controlled(
        &mut self,
        remote_path: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, String> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("FTP download cancelled".to_string());
        }

        // Set binary mode
        self.conn.type_image().await?;

        // Get file size for pre-allocation
        let file_size = self.conn.size(remote_path).await.ok();

        // Set resume offset if specified
        if let Some(offset) = self.options.resume_offset
            && offset > 0
        {
            self.conn.rest(offset).await?;
        }

        // Establish data connection
        let data_connection = self.establish_data_connection().await?;

        // Initiate RETR command
        self.conn.retr(remote_path).await?;

        // Receive data into memory
        let data = self
            .receive_data_to_memory(data_connection, file_size, cancellation)
            .await?;

        Ok(data)
    }

    /// Download a directory recursively (if recursive_download is enabled)
    ///
    /// Returns results for each file downloaded
    pub async fn download_directory(
        &mut self,
        remote_dir: &str,
        local_base_dir: &str,
        progress_callback: Option<fn(DownloadProgress)>,
    ) -> Result<Vec<DownloadResult>, String> {
        self.download_directory_controlled(remote_dir, local_base_dir, progress_callback, None)
            .await
    }

    /// Download a directory recursively while observing a cancellation token.
    pub async fn download_directory_with_cancellation(
        &mut self,
        remote_dir: &str,
        local_base_dir: &str,
        progress_callback: Option<fn(DownloadProgress)>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DownloadResult>, String> {
        self.download_directory_controlled(
            remote_dir,
            local_base_dir,
            progress_callback,
            Some(cancellation),
        )
        .await
    }

    async fn download_directory_controlled(
        &mut self,
        remote_dir: &str,
        local_base_dir: &str,
        progress_callback: Option<fn(DownloadProgress)>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<DownloadResult>, String> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("FTP download cancelled".to_string());
        }

        if !self.options.recursive_download {
            return Err("Recursive download not enabled in options".to_string());
        }

        // Change to remote directory
        self.conn.cwd(remote_dir).await?;

        // Negotiate the data channel before sending LIST. The server opens the
        // negotiated channel only after it receives the transfer command.
        let data_connection = self.establish_data_connection().await?;

        let list_resp = self.conn.list(None).await?;
        if !list_resp.is_positive_preliminary() {
            return Err(format!(
                "LIST failed: {} {}",
                list_resp.code, list_resp.message
            ));
        }

        // Read directory listing from data connection
        let listing_data = self
            .receive_data_to_memory(data_connection, None, cancellation)
            .await?;

        // Parse listing
        let listing_str = String::from_utf8_lossy(&listing_data);
        let entries = parse_ftp_list_response(&listing_str);

        // Create local directory
        std::fs::create_dir_all(local_base_dir)
            .map_err(|e| format!("Failed to create local directory: {}", e))?;

        let mut results = Vec::new();
        for entry in entries {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err("FTP download cancelled".to_string());
            }

            if entry.is_directory {
                // Recursively download subdirectory
                let sub_remote = format!("{}/{}", remote_dir.trim_end_matches('/'), entry.name);
                let sub_local = format!("{}/{}", local_base_dir.trim_end_matches('/'), entry.name);

                let sub_results = Box::pin(self.download_directory_controlled(
                    &sub_remote,
                    &sub_local,
                    progress_callback,
                    cancellation,
                ))
                .await?;
                results.extend(sub_results);
            } else {
                // Download individual file
                let remote_file = format!("{}/{}", remote_dir.trim_end_matches('/'), entry.name);
                let local_file = format!("{}/{}", local_base_dir.trim_end_matches('/'), entry.name);

                let result = self
                    .download_file_controlled(
                        &remote_file,
                        &local_file,
                        progress_callback,
                        cancellation,
                    )
                    .await?;
                results.push(result);
            }
        }

        Ok(results)
    }

    /// Establish data connection using configured mode (passive/active)
    async fn establish_data_connection(&mut self) -> Result<FtpDataConnection, String> {
        if self.conn.options.passive_mode {
            // Try EPSV first (supports IPv6), fallback to PASV
            match self.conn.epsv().await {
                Ok(port) => {
                    // For EPSV, use same host as control connection
                    Ok(FtpDataConnection::Passive {
                        host: self.conn.host.clone(),
                        port,
                    })
                }
                Err(_) => {
                    // Fallback to PASV
                    let (host, port) = self.conn.pasv().await?;
                    Ok(FtpDataConnection::Passive { host, port })
                }
            }
        } else {
            // Active mode
            match self.conn.prepare_eprt_active().await {
                Ok(listener) => Ok(FtpDataConnection::Active(listener)),
                Err(_) => {
                    // Try PORT for IPv4
                    let listener = self.conn.prepare_port_active().await?;
                    Ok(FtpDataConnection::Active(listener))
                }
            }
        }
    }

    /// Receive data stream and write to file with error handling and retries
    async fn receive_data_to_file(
        &mut self,
        data_connection: FtpDataConnection,
        local_path: &str,
        file_size: Option<u64>,
        progress_callback: Option<fn(DownloadProgress)>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DownloadResult, String> {
        let mut data_stream = data_connection
            .open(self.options.data_connect_timeout)
            .await?;

        // Open/create local file
        let has_resume_offset = self.options.resume_offset.is_some_and(|offset| offset > 0);
        let mut file = if has_resume_offset {
            tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(local_path)
                .await
                .map_err(|e| format!("Failed to open local file for resume: {}", e))?
        } else {
            tokio::fs::File::create(local_path)
                .await
                .map_err(|e| format!("Failed to create local file: {}", e))?
        };

        // Seek to resume offset if resuming
        if let Some(offset) = self.options.resume_offset
            && offset > 0
        {
            file.seek(SeekFrom::Start(offset))
                .await
                .map_err(|e| format!("Failed to seek file: {}", e))?;
        }

        // Data receive loop with retry logic
        let mut buffer = vec![0u8; self.options.buffer_size];
        let mut total_downloaded = self.options.resume_offset.unwrap_or(0);
        let start_time = std::time::Instant::now();
        let mut read_retry_count = 0u32;
        loop {
            let read_result = if let Some(token) = cancellation {
                tokio::select! {
                    _ = token.cancelled() => {
                        drop(data_stream);
                        let _ = self.conn.abor().await;
                        return Err("FTP download cancelled".to_string());
                    }
                    result = data_stream.read(&mut buffer) => result,
                }
            } else {
                data_stream.read(&mut buffer).await
            };

            match read_result {
                Ok(bytes_read) => {
                    read_retry_count = 0; // Reset retry counter on success

                    if bytes_read == 0 {
                        break; // End of stream
                    }

                    // Write to local file
                    file.write_all(&buffer[..bytes_read])
                        .await
                        .map_err(|e| format!("Failed to write to local file: {}", e))?;

                    total_downloaded += bytes_read as u64;

                    // Report progress
                    if let Some(cb) = progress_callback {
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 {
                            total_downloaded as f64 / elapsed
                        } else {
                            0.0
                        };
                        cb(DownloadProgress {
                            downloaded_bytes: total_downloaded,
                            total_bytes: file_size,
                            speed_bytes_per_sec: speed,
                        });
                    }
                }
                Err(ref e)
                    if is_transient_io_error(e) && read_retry_count < self.options.max_retries =>
                {
                    // Transient error - retry with exponential backoff
                    read_retry_count += 1;
                    let wait_ms = 1000u64 * (1 << (read_retry_count - 1));
                    warn!(
                        "FTP read error (#{}), retrying in {}ms...",
                        read_retry_count, wait_ms
                    );
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    continue;
                }
                Err(e) => {
                    return Err(format!(
                        "FTP data read failed after {} retries: {}",
                        read_retry_count, e
                    ));
                }
            }
        }

        // Flush and close file
        file.flush()
            .await
            .map_err(|e| format!("Failed to flush file: {}", e))?;
        drop(data_stream); // Close data connection

        self.read_transfer_complete().await?;

        // Calculate statistics
        let elapsed = start_time.elapsed().as_secs_f64();
        let avg_speed = if elapsed > 0.0 {
            total_downloaded as f64 / elapsed
        } else {
            0.0
        };

        Ok(DownloadResult {
            file_path: local_path.to_string(),
            bytes_downloaded: total_downloaded,
            total_size: file_size,
            success: true,
            average_speed_bps: avg_speed,
            duration_secs: elapsed,
        })
    }

    /// Receive data stream into memory buffer
    async fn receive_data_to_memory(
        &mut self,
        data_connection: FtpDataConnection,
        expected_size: Option<u64>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, String> {
        let mut data_stream = data_connection
            .open(self.options.data_connect_timeout)
            .await?;

        // Pre-allocate buffer based on expected size
        let capacity = expected_size.unwrap_or(1024 * 1024) as usize;
        let mut result = Vec::with_capacity(capacity);
        let mut buffer = vec![0u8; self.options.buffer_size];

        let mut read_retry_count = 0u32;
        loop {
            let read_result = if let Some(token) = cancellation {
                tokio::select! {
                    _ = token.cancelled() => {
                        drop(data_stream);
                        let _ = self.conn.abor().await;
                        return Err("FTP download cancelled".to_string());
                    }
                    result = data_stream.read(&mut buffer) => result,
                }
            } else {
                data_stream.read(&mut buffer).await
            };

            match read_result {
                Ok(0) => break,
                Ok(bytes_read) => {
                    read_retry_count = 0;
                    result.extend_from_slice(&buffer[..bytes_read]);
                }
                Err(ref e)
                    if is_transient_io_error(e) && read_retry_count < self.options.max_retries =>
                {
                    read_retry_count += 1;
                    let wait_ms = 1000u64 * (1 << (read_retry_count - 1));
                    warn!(
                        "FTP memory read error (#{}), retrying in {}ms...",
                        read_retry_count, wait_ms
                    );
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
                Err(e) => {
                    return Err(format!(
                        "FTP memory read failed after {} retries: {}",
                        read_retry_count, e
                    ));
                }
            }
        }

        drop(data_stream);

        self.read_transfer_complete().await?;

        Ok(result)
    }

    async fn read_transfer_complete(&mut self) -> Result<(), String> {
        let final_resp = self.conn.read_response().await?;
        if final_resp.class() != FtpResponseClass::PositiveCompletion
            && final_resp.class() != FtpResponseClass::PositivePreliminary
        {
            return Err(format!(
                "FTP transfer failed: {} {}",
                final_resp.code, final_resp.message
            ));
        }
        Ok(())
    }
}
