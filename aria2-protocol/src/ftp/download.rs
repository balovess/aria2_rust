use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;
use tracing::warn;

const MAX_MEMORY_DOWNLOAD_PREALLOCATION: usize = 1024 * 1024;

use super::connection::{FtpActiveDataListener, FtpConnection, FtpResponseClass};

mod directory;
mod types;
#[cfg(test)]
use directory::local_child_path;
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

fn memory_download_capacity(expected_size: Option<u64>) -> usize {
    expected_size
        .map(|size| {
            usize::try_from(size)
                .unwrap_or(MAX_MEMORY_DOWNLOAD_PREALLOCATION)
                .min(MAX_MEMORY_DOWNLOAD_PREALLOCATION)
        })
        .unwrap_or(MAX_MEMORY_DOWNLOAD_PREALLOCATION)
}

fn validate_transfer_size(expected_size: Option<u64>, actual_size: u64) -> Result<(), String> {
    if let Some(expected_size) = expected_size
        && actual_size != expected_size
    {
        return Err(format!(
            "FTP transfer size mismatch: expected {expected_size} bytes, received {actual_size} bytes"
        ));
    }
    Ok(())
}

async fn prepare_resume_target(
    file: &mut tokio::fs::File,
    resume_offset: u64,
) -> Result<(), String> {
    let local_size = file
        .metadata()
        .await
        .map_err(|error| format!("Failed to inspect local resume file: {error}"))?
        .len();
    if local_size < resume_offset {
        return Err(format!(
            "FTP resume offset {resume_offset} exceeds local file length {local_size}"
        ));
    }
    if local_size > resume_offset {
        file.set_len(resume_offset)
            .await
            .map_err(|error| format!("Failed to truncate local resume file: {error}"))?;
    }
    file.seek(SeekFrom::Start(resume_offset))
        .await
        .map_err(|error| format!("Failed to seek local resume file: {error}"))?;
    Ok(())
}

impl<'a> FtpDownload<'a> {
    /// Create a new FTP download manager
    pub fn new(conn: &'a mut FtpConnection, options: Option<FtpDownloadOptions>) -> Self {
        let options = options.unwrap_or_else(|| FtpDownloadOptions {
            max_retries: conn.options.max_retries,
            ..FtpDownloadOptions::default()
        });
        Self { conn, options }
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
        let mut data_stream = match data_connection
            .open(self.options.data_connect_timeout)
            .await
        {
            Ok(stream) => stream,
            Err(error) => return Err(self.abort_after_transfer_error(error).await),
        };

        // Open/create local file
        let has_resume_offset = self.options.resume_offset.is_some_and(|offset| offset > 0);
        let file = if has_resume_offset {
            tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(local_path)
                .await
                .map_err(|e| format!("Failed to open local file for resume: {}", e))
        } else {
            tokio::fs::File::create(local_path)
                .await
                .map_err(|e| format!("Failed to create local file: {}", e))
        };
        let mut file = match file {
            Ok(file) => file,
            Err(error) => {
                drop(data_stream);
                return Err(self.abort_after_transfer_error(error).await);
            }
        };

        // Seek to resume offset if resuming
        if let Some(offset) = self.options.resume_offset
            && offset > 0
            && let Err(error) = prepare_resume_target(&mut file, offset).await
        {
            drop(data_stream);
            return Err(self.abort_after_transfer_error(error).await);
        }

        // Data receive loop with retry logic
        let mut buffer = vec![0u8; self.options.effective_buffer_size()];
        let mut total_downloaded = self.options.resume_offset.unwrap_or(0);
        let start_time = std::time::Instant::now();
        let mut read_retry_count = 0u32;
        loop {
            let read_result = if let Some(token) = cancellation {
                tokio::select! {
                    _ = token.cancelled() => {
                        drop(data_stream);
                        return Err(self.abort_after_transfer_error(
                            "FTP download cancelled".to_string(),
                        ).await);
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
                    if let Err(error) = file.write_all(&buffer[..bytes_read]).await {
                        drop(data_stream);
                        return Err(self
                            .abort_after_transfer_error(format!(
                                "Failed to write to local file: {error}"
                            ))
                            .await);
                    }

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
                    drop(data_stream);
                    return Err(self
                        .abort_after_transfer_error(format!(
                            "FTP data read failed after {} retries: {}",
                            read_retry_count, e
                        ))
                        .await);
                }
            }
        }

        // Flush and close file
        if let Err(error) = file.flush().await {
            drop(data_stream);
            let _ = self.read_transfer_complete().await;
            return Err(format!("Failed to flush file: {error}"));
        }
        drop(data_stream); // Close data connection

        self.read_transfer_complete().await?;
        validate_transfer_size(file_size, total_downloaded)?;

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
        let mut data_stream = match data_connection
            .open(self.options.data_connect_timeout)
            .await
        {
            Ok(stream) => stream,
            Err(error) => return Err(self.abort_after_transfer_error(error).await),
        };

        // The remote SIZE is untrusted; cap eager allocation and grow as data arrives.
        let capacity = memory_download_capacity(expected_size);
        let mut result = Vec::with_capacity(capacity);
        let mut buffer = vec![0u8; self.options.effective_buffer_size()];

        let mut read_retry_count = 0u32;
        loop {
            let read_result = if let Some(token) = cancellation {
                tokio::select! {
                    _ = token.cancelled() => {
                        drop(data_stream);
                        return Err(self.abort_after_transfer_error(
                            "FTP download cancelled".to_string(),
                        ).await);
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
                    drop(data_stream);
                    return Err(self
                        .abort_after_transfer_error(format!(
                            "FTP memory read failed after {} retries: {}",
                            read_retry_count, e
                        ))
                        .await);
                }
            }
        }

        drop(data_stream);

        self.read_transfer_complete().await?;
        let actual_size = self
            .options
            .resume_offset
            .unwrap_or(0)
            .saturating_add(result.len() as u64);
        validate_transfer_size(expected_size, actual_size)?;

        Ok(result)
    }

    async fn read_transfer_complete(&mut self) -> Result<(), String> {
        let final_resp = self.conn.read_response().await?;
        if final_resp.class() != FtpResponseClass::PositiveCompletion {
            return Err(format!(
                "FTP transfer failed: {} {}",
                final_resp.code, final_resp.message
            ));
        }
        Ok(())
    }

    async fn abort_after_transfer_error(&mut self, error: String) -> String {
        if let Err(abort_error) = self.conn.abor().await {
            warn!(%abort_error, "Failed to synchronize FTP control session after transfer error");
        }
        error
    }
}
