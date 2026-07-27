//! FTP data transfer operations
//!
//! Contains methods for directory listing and file download,
//! which involve establishing data connections and reading data streams.

use crate::error::{Aria2Error, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::types::{FtpClient, FtpFileInfo, FtpMode};

impl FtpClient {
    /// List directory contents
    ///
    /// Supports two formats:
    /// - MLSD (machine-readable listing, if the server supports it)
    /// - LIST (traditional Unix/Windows format)
    ///
    /// # Arguments
    ///
    /// - `path`: Directory path to list
    ///
    /// # Returns
    ///
    /// Returns a vector of file info
    ///
    /// # Errors
    ///
    /// - 550 Directory does not exist or is not accessible
    /// - 425/426 Data connection error
    pub async fn list_directory(&mut self, path: &str) -> Result<Vec<FtpFileInfo>> {
        debug!("Listing directory: {}", path);

        // Establish data connection based on current mode
        let mut data_stream = match self.mode {
            FtpMode::Passive => {
                // Passive mode first, fallback to active mode on failure
                match self.passive_mode().await {
                    Ok(stream) => stream,
                    Err(e) => {
                        warn!("Passive mode failed, trying active mode: {}", e);
                        self.active_mode().await?
                    }
                }
            }
            FtpMode::Active => self.active_mode().await?,
        };

        // Try MLSD first (machine-readable format)
        self.send_command(&format!("MLSD {}", path)).await?;
        let resp = self.read_response().await?;

        let use_mlsd = resp.is_positive_preliminary();

        if !use_mlsd {
            // MLSD unavailable, use LIST
            self.send_command(&format!("LIST {}", path)).await?;
            let list_resp = self.read_response().await?;

            if !list_resp.is_positive_preliminary() {
                if list_resp.code == 550 {
                    return Err(Aria2Error::Recoverable(
                        crate::error::RecoverableError::ServerError { code: 550 },
                    ));
                }
                return Err(Aria2Error::DownloadFailed(format!(
                    "LIST command failed: {} {}",
                    list_resp.code, list_resp.message
                )));
            }
        }

        // Read data stream
        let mut buffer = String::new();
        let bytes_read = timeout(self.read_timeout, data_stream.read_to_string(&mut buffer))
            .await
            .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
            .map_err(|e| Aria2Error::Io(format!("Failed to read directory listing: {}", e)))?;

        drop(data_stream); // Close data connection

        debug!("Read {} bytes of directory listing", bytes_read);

        // Read final response
        let final_resp = self.read_response().await?;
        if final_resp.code == 426 {
            return Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 426 },
            ));
        } else if !final_resp.is_positive_completion() {
            return Err(Aria2Error::DownloadFailed(format!(
                "Directory listing transfer completed but returned error: {} {}",
                final_resp.code, final_resp.message
            )));
        }

        // Parse directory listing
        let files: Vec<FtpFileInfo> = buffer
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with("total:") {
                    return None;
                }
                Self::parse_list_line(line)
            })
            .collect();

        debug!("Parsed {} file/directory entries", files.len());
        Ok(files)
    }

    /// Download a file
    ///
    /// Establishes a data connection, optionally sets a resume offset, then
    /// sends RETR to begin the file transfer. The caller reads file contents
    /// from the returned `TcpStream`.
    ///
    /// # Command Order (matching C++ FtpNegotiationCommand)
    ///
    /// 1. PASV / PORT — establish data connection
    /// 2. REST — set resume offset (if offset > 0)
    /// 3. RETR — begin file transfer
    ///
    /// The C++ `FtpNegotiationCommand` follows the sequence:
    /// `SEQ_PREPARE_PASV` → `SEQ_SEND_REST` → `SEQ_SEND_RETR`.
    /// REST must come **after** the data connection method so that some
    /// servers (which reset their data-port state on REST) remain consistent.
    ///
    /// # Arguments
    ///
    /// - `remote_path`: Remote file path
    /// - `offset`: Optional starting offset (for resume transfer)
    ///
    /// # Returns
    ///
    /// Returns the data connection `TcpStream` for reading file contents.
    /// The caller is responsible for reading data and closing the stream.
    ///
    /// # Errors
    ///
    /// - 550 File not found
    /// - 425/426 Data connection error
    pub async fn download_file(
        &mut self,
        remote_path: &str,
        offset: Option<u64>,
    ) -> Result<TcpStream> {
        debug!(
            "Preparing to download file: {} (offset: {:?})",
            remote_path, offset
        );

        // Step 1: Establish data connection (PASV/PORT)
        // C++ FtpNegotiationCommand: SEQ_PREPARE_PASV / SEQ_PREPARE_PORT
        let data_stream = match self.mode {
            FtpMode::Passive => match self.passive_mode().await {
                Ok(stream) => stream,
                Err(e) => {
                    warn!("Passive mode failed, trying active mode: {}", e);
                    self.active_mode().await?
                }
            },
            FtpMode::Active => self.active_mode().await?,
        };

        // Step 2: Send REST if resuming (after data connection, before RETR)
        // C++ FtpNegotiationCommand: SEQ_SEND_REST → SEQ_RECV_REST
        if let Some(off) = offset
            && off > 0
        {
            debug!("Setting resume offset: {}", off);
            self.send_command(&format!("REST {}", off)).await?;
            let rest_resp = self.read_response().await?;

            if rest_resp.code != 350 {
                return Err(Aria2Error::DownloadFailed(format!(
                    "REST command failed (server may not support resume transfer): {} {}",
                    rest_resp.code, rest_resp.message
                )));
            }
        }

        // Step 3: Send RETR command
        // C++ FtpNegotiationCommand: SEQ_SEND_RETR → SEQ_RECV_RETR
        self.send_command(&format!("RETR {}", remote_path)).await?;
        let retr_resp = self.read_response().await?;

        if !retr_resp.is_positive_preliminary() {
            if retr_resp.code == 550 {
                return Err(Aria2Error::Recoverable(
                    crate::error::RecoverableError::ServerError { code: 550 },
                ));
            }
            return Err(Aria2Error::DownloadFailed(format!(
                "RETR command failed: {} {}",
                retr_resp.code, retr_resp.message
            )));
        }

        debug!("Data connection ready for file download: {}", remote_path);
        Ok(data_stream)
    }
}
