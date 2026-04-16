use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::warn;

use super::connection::{FtpConnection, FtpResponseClass};
use super::listing::parse_ftp_list_response;

const DEFAULT_BUFFER_SIZE: usize = 65536;

/// FTP download configuration options
#[derive(Debug, Clone)]
pub struct FtpDownloadOptions {
    pub buffer_size: usize,
    pub resume_offset: Option<u64>,
    pub max_retries: u32,
    /// Transfer mode (binary or ASCII)
    pub binary_mode: bool,
    /// Timeout for data connection establishment
    pub data_connect_timeout: Duration,
    /// Whether to download directories recursively
    pub recursive_download: bool,
}

impl Default for FtpDownloadOptions {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_BUFFER_SIZE,
            resume_offset: None,
            max_retries: 3,
            binary_mode: true,
            data_connect_timeout: Duration::from_secs(30),
            recursive_download: false,
        }
    }
}

/// Check if an IO error is transient and retry-worthy
fn is_transient_io_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
    ) || e.to_string().to_lowercase().contains("temporary")
}

/// Download progress information
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: f64,
}

/// Result of a file download operation
#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub file_path: String,
    pub bytes_downloaded: u64,
    pub total_size: Option<u64>,
    pub success: bool,
    pub average_speed_bps: f64,
    pub duration_secs: f64,
}

impl DownloadResult {
    /// Check if the download completed successfully with all bytes received
    pub fn is_complete(&self) -> bool {
        self.success
            && match self.total_size {
                Some(total) => self.bytes_downloaded >= total,
                None => self.bytes_downloaded > 0,
            }
    }

    /// Convert byte count to human-readable string
    pub fn human_readable_size(bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        let mut size = bytes as f64;
        let mut unit_idx = 0;
        while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
            size /= 1024.0;
            unit_idx += 1;
        }
        if unit_idx == 0 {
            format!("{} {}", bytes, UNITS[unit_idx])
        } else {
            format!("{:.2} {}", size, UNITS[unit_idx])
        }
    }
}

/// FTP download manager that handles file transfers
pub struct FtpDownload<'a> {
    conn: &'a mut FtpConnection,
    options: FtpDownloadOptions,
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
        let (data_host, data_port) = self.establish_data_connection().await?;

        // Send RETR command to initiate transfer
        self.conn.retr(remote_path).await?;

        // Connect to data port and receive file content
        let result = self
            .receive_data_to_file(
                &data_host,
                data_port,
                local_path,
                file_size,
                progress_callback,
            )
            .await?;

        Ok(result)
    }

    /// Download a file into memory (for small files or when disk I/O is not needed)
    pub async fn download_to_memory(&mut self, remote_path: &str) -> Result<Vec<u8>, String> {
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
        let (data_host, data_port) = self.establish_data_connection().await?;

        // Initiate RETR command
        self.conn.retr(remote_path).await?;

        // Receive data into memory
        let data = self
            .receive_data_to_memory(&data_host, data_port, file_size)
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
        if !self.options.recursive_download {
            return Err("Recursive download not enabled in options".to_string());
        }

        // Change to remote directory
        self.conn.cwd(remote_dir).await?;

        // Request directory listing
        let list_resp = self.conn.list(None).await?;

        if list_resp.code != 150 && list_resp.code != 125 && list_resp.code != 226 {
            // If LIST didn't open data connection, we need to handle it differently
            // For now, assume listing will come through data channel
        }

        // Establish data connection for LIST response
        let (data_host, data_port) = self.establish_data_connection().await?;

        // Read directory listing from data connection
        let listing_data = self
            .receive_data_to_memory(&data_host, data_port, None)
            .await?;

        // Parse listing
        let listing_str = String::from_utf8_lossy(&listing_data);
        let entries = parse_ftp_list_response(&listing_str);

        // Create local directory
        std::fs::create_dir_all(local_base_dir)
            .map_err(|e| format!("Failed to create local directory: {}", e))?;

        let mut results = Vec::new();
        for entry in entries {
            if entry.is_directory {
                // Recursively download subdirectory
                let sub_remote = format!("{}/{}", remote_dir.trim_end_matches('/'), entry.name);
                let sub_local = format!("{}/{}", local_base_dir.trim_end_matches('/'), entry.name);

                let sub_results =
                    Box::pin(self.download_directory(&sub_remote, &sub_local, progress_callback))
                        .await?;
                results.extend(sub_results);
            } else {
                // Download individual file
                let remote_file = format!("{}/{}", remote_dir.trim_end_matches('/'), entry.name);
                let local_file = format!("{}/{}", local_base_dir.trim_end_matches('/'), entry.name);

                let result = self
                    .download_file(&remote_file, &local_file, progress_callback)
                    .await?;
                results.push(result);
            }
        }

        Ok(results)
    }

    /// Establish data connection using configured mode (passive/active)
    async fn establish_data_connection(&mut self) -> Result<(String, u16), String> {
        if self.conn.options.passive_mode {
            // Try EPSV first (supports IPv6), fallback to PASV
            match self.conn.epsv().await {
                Ok(port) => {
                    // For EPSV, use same host as control connection
                    Ok((self.conn.host.clone(), port))
                }
                Err(_) => {
                    // Fallback to PASV
                    self.conn.pasv().await
                }
            }
        } else {
            // Active mode
            match self.conn.eprt_active().await {
                Ok((host, port)) => Ok((host, port)),
                Err(_) => {
                    // Try PORT for IPv4
                    let port = self.conn.port_active().await?;
                    Ok(("127.0.0.1".to_string(), port))
                }
            }
        }
    }

    /// Receive data stream and write to file with error handling and retries
    async fn receive_data_to_file(
        &mut self,
        data_host: &str,
        data_port: u16,
        local_path: &str,
        file_size: Option<u64>,
        progress_callback: Option<fn(DownloadProgress)>,
    ) -> Result<DownloadResult, String> {
        // Connect to data port with timeout
        let mut data_stream: TcpStream = timeout(
            self.options.data_connect_timeout,
            TcpStream::connect((data_host, data_port)),
        )
        .await
        .map_err(|_| {
            format!(
                "FTP data connection timeout ({}s)",
                self.options.data_connect_timeout.as_secs()
            )
        })?
        .map_err(|e| format!("FTP data connection failed: {}", e))?;

        // Open/create local file
        let mut file = tokio::fs::File::create(local_path)
            .await
            .map_err(|e| format!("Failed to create local file: {}", e))?;

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
        const MAX_READ_RETRIES: u32 = 3;

        loop {
            let read_result = data_stream.read(&mut buffer).await;

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
                Err(ref e) if is_transient_io_error(e) && read_retry_count < MAX_READ_RETRIES => {
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

        // Read final response from control channel
        let final_resp = self.conn.read_response().await?;
        if final_resp.class() != FtpResponseClass::PositiveCompletion
            && final_resp.class() != FtpResponseClass::PositivePreliminary
        {
            return Err(format!(
                "Download completed but server reported error: {} {}",
                final_resp.code, final_resp.message
            ));
        }

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
        data_host: &str,
        data_port: u16,
        expected_size: Option<u64>,
    ) -> Result<Vec<u8>, String> {
        // Connect to data port
        let mut data_stream = timeout(
            self.options.data_connect_timeout,
            TcpStream::connect((data_host, data_port)),
        )
        .await
        .map_err(|_| "FTP data connection timeout")?
        .map_err(|e| format!("FTP data connection failed: {}", e))?;

        // Pre-allocate buffer based on expected size
        let capacity = expected_size.unwrap_or(1024 * 1024) as usize;
        let mut result = Vec::with_capacity(capacity);
        let mut buffer = vec![0u8; self.options.buffer_size];

        loop {
            let bytes_read = data_stream
                .read(&mut buffer)
                .await
                .map_err(|e| format!("FTP data read error: {}", e))?;

            if bytes_read == 0 {
                break;
            }

            result.extend_from_slice(&buffer[..bytes_read]);
        }

        drop(data_stream);

        // Read final response (may timeout for some servers)
        let _final_resp = self.conn.read_response().await.ok(); // Ignore errors

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human_readable_size() {
        assert_eq!(DownloadResult::human_readable_size(500), "500 B");
        assert_eq!(DownloadResult::human_readable_size(1024), "1.00 KB");
        assert_eq!(DownloadResult::human_readable_size(1536), "1.50 KB");
        assert_eq!(DownloadResult::human_readable_size(1048576), "1.00 MB");
        assert_eq!(DownloadResult::human_readable_size(1073741824), "1.00 GB");
    }

    #[test]
    fn test_download_result_complete() {
        let full = DownloadResult {
            file_path: "test.bin".into(),
            bytes_downloaded: 1000,
            total_size: Some(1000),
            success: true,
            average_speed_bps: 1000.0,
            duration_secs: 1.0,
        };
        assert!(full.is_complete());

        let partial = DownloadResult {
            file_path: "test.bin".into(),
            bytes_downloaded: 500,
            total_size: Some(1000),
            success: true,
            average_speed_bps: 500.0,
            duration_secs: 1.0,
        };
        assert!(!partial.is_complete());

        let unknown_total = DownloadResult {
            file_path: "test.bin".into(),
            bytes_downloaded: 100,
            total_size: None,
            success: true,
            average_speed_bps: 100.0,
            duration_secs: 1.0,
        };
        assert!(unknown_total.is_complete()); // Any data with unknown total is complete

        let failed = DownloadResult {
            file_path: "test.bin".into(),
            bytes_downloaded: 1000,
            total_size: Some(1000),
            success: false, // Failed
            average_speed_bps: 1000.0,
            duration_secs: 1.0,
        };
        assert!(!failed.is_complete());
    }

    #[test]
    fn test_ftp_download_options_default() {
        let opts = FtpDownloadOptions::default();
        assert_eq!(opts.buffer_size, DEFAULT_BUFFER_SIZE);
        assert!(opts.resume_offset.is_none());
        assert_eq!(opts.max_retries, 3);
        assert!(opts.binary_mode);
        assert_eq!(opts.data_connect_timeout, Duration::from_secs(30));
        assert!(!opts.recursive_download);
    }

    #[test]
    fn test_is_transient_io_error() {
        use std::io::ErrorKind;

        // Transient errors (should retry)
        let interrupted = std::io::Error::new(ErrorKind::Interrupted, "interrupted");
        assert!(is_transient_io_error(&interrupted));

        let would_block = std::io::Error::new(ErrorKind::WouldBlock, "would block");
        assert!(is_transient_io_error(&would_block));

        let connection_reset = std::io::Error::new(ErrorKind::ConnectionReset, "connection reset");
        assert!(is_transient_io_error(&connection_reset));

        let timed_out = std::io::Error::new(ErrorKind::TimedOut, "timed out");
        assert!(is_transient_io_error(&timed_out));

        // Non-transient errors (should not retry)
        let not_found = std::io::Error::new(ErrorKind::NotFound, "not found");
        assert!(!is_transient_io_error(&not_found));

        let permission_denied =
            std::io::Error::new(ErrorKind::PermissionDenied, "permission denied");
        assert!(!is_transient_io_error(&permission_denied));

        // Error with "temporary" in message
        let temp_error = std::io::Error::other("temporary failure");
        assert!(is_transient_io_error(&temp_error));
    }
}
