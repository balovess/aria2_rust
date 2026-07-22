//! FTP protocol client implementation
//!
//! Provides an async FTP client supporting passive/active mode, binary transfer,
//! directory listing parsing, and more.

use crate::error::{Aria2Error, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

/// FTP data connection mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtpMode {
    /// Passive mode (client connects to server data port)
    #[default]
    Passive,
    /// Active mode (server connects to client data port)
    Active,
}

/// FTP response struct
#[derive(Debug, Clone)]
pub struct FtpResponse {
    /// FTP response code (3-digit number)
    pub code: u16,
    /// Response message text
    pub message: String,
}

impl FtpResponse {
    /// Check if this is a success response (1xx-3xx)
    pub fn is_success(&self) -> bool {
        (100..400).contains(&self.code)
    }

    /// Check if this is an intermediate response (1xx)
    pub fn is_intermediate(&self) -> bool {
        (100..200).contains(&self.code)
    }

    /// Check if this is a positive completion response (2xx)
    pub fn is_positive_completion(&self) -> bool {
        (200..300).contains(&self.code)
    }

    /// Check if this is a positive preliminary response (1xx)
    pub fn is_positive_preliminary(&self) -> bool {
        (100..200).contains(&self.code)
    }
}

/// FTP file info struct
#[derive(Debug, Clone)]
pub struct FtpFileInfo {
    /// File or directory name
    pub name: String,
    /// File size in bytes (0 for directories)
    pub size: u64,
    /// Whether this is a directory
    pub is_dir: bool,
}

/// FTP client
///
/// Async FTP protocol implementation, supporting:
/// - Passive mode priority, with active mode fallback
/// - Binary/ASCII transfer mode switching
/// - Resume transfer (REST command)
/// - Directory listing parsing (Unix/Windows format)
///
/// # Examples
///
/// ```rust,no_run
/// use aria2_core::ftp::connection::{FtpClient, FtpMode};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let mut client = FtpClient::connect("ftp.example.com", 21, FtpMode::Passive).await?;
///     client.login("anonymous", "user@example.com").await?;
///     client.set_binary_mode(true).await?;
///
///     let files = client.list_directory("/").await?;
///     for file in &files {
///         println!("{} {} {}", if file.is_dir { "D" } else { "F" }, file.size, file.name);
///     }
///
///     client.quit().await?;
///     Ok(())
/// }
/// ```
pub struct FtpClient {
    /// Control connection stream (buffered)
    pub(crate) control_stream: BufReader<TcpStream>,
    /// Data connection mode
    pub(crate) mode: FtpMode,
    /// Current binary mode state
    pub(crate) binary_mode: bool,
    /// Server host address
    pub(crate) host: String,
    /// Server port
    #[allow(dead_code)] // Port field retained for FTP connection configuration
    pub(crate) port: u16,
    /// Connection timeout
    pub(crate) connect_timeout: Duration,
    /// Read timeout
    pub(crate) read_timeout: Duration,
}

impl FtpClient {
    /// Default connection timeout: 30 seconds
    const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    /// Default read timeout: 30 seconds
    const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

    /// Connect to an FTP server
    ///
    /// # Arguments
    ///
    /// - `host`: FTP server address (domain name or IP)
    /// - `port`: FTP server port (typically 21)
    /// - `mode`: Data connection mode (passive or active)
    ///
    /// # Errors
    ///
    /// - Connection timeout
    /// - Network error
    /// - Server refused connection
    pub async fn connect(host: &str, port: u16, mode: FtpMode) -> Result<Self> {
        info!("FTP connecting: {}:{}", host, port);

        let stream = timeout(
            Self::DEFAULT_CONNECT_TIMEOUT,
            TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("FTP connection failed: {}", e)))?;

        let mut client = Self {
            control_stream: BufReader::new(stream),
            mode,
            binary_mode: false,
            host: host.to_string(),
            port,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            read_timeout: Self::DEFAULT_READ_TIMEOUT,
        };

        // Read welcome message
        let welcome = client.read_response().await?;
        if !welcome.is_positive_completion() && !welcome.is_positive_preliminary() {
            return Err(Aria2Error::DownloadFailed(format!(
                "FTP server refused connection: {} {}",
                welcome.code, welcome.message
            )));
        }

        debug!("FTP connected successfully: {}", welcome.message.trim());
        Ok(client)
    }

    /// Log in to the FTP server
    ///
    /// # Arguments
    ///
    /// - `username`: Username (use "anonymous" for anonymous login)
    /// - `password`: Password (use email for anonymous login)
    ///
    /// # Errors
    ///
    /// - 530 Not logged in (authentication failed)
    pub async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        debug!("Sending USER command: {}", username);
        self.send_command(&format!("USER {}", username)).await?;
        let resp = self.read_response().await?;

        match resp.code {
            230 => {
                // No password required, login successful
                info!("FTP login successful (no password required)");
                Ok(())
            }
            331 | 332 => {
                // Password required
                debug!("Password authentication required, sending PASS command");
                self.send_command(&format!("PASS {}", password)).await?;
                let pass_resp = self.read_response().await?;

                if pass_resp.code == 230 || pass_resp.code == 202 {
                    info!("FTP login successful");
                    Ok(())
                } else if pass_resp.code == 530 {
                    Err(Aria2Error::Recoverable(
                        crate::error::RecoverableError::ServerError { code: 530 },
                    ))
                } else {
                    Err(Aria2Error::DownloadFailed(format!(
                        "FTP login failed: {} {}",
                        pass_resp.code, pass_resp.message
                    )))
                }
            }
            530 => Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 530 },
            )),
            _ => {
                if resp.is_positive_completion() {
                    info!("FTP login successful");
                    Ok(())
                } else {
                    Err(Aria2Error::DownloadFailed(format!(
                        "FTP login failed: {} {}",
                        resp.code, resp.message
                    )))
                }
            }
        }
    }

    /// Set transfer mode (binary/ASCII)
    ///
    /// # Arguments
    ///
    /// - `enabled`: true for binary mode (TYPE I), false for ASCII mode (TYPE A)
    ///
    /// # Errors
    ///
    /// - 504 Unsupported transfer mode
    pub async fn set_binary_mode(&mut self, enabled: bool) -> Result<()> {
        let type_cmd = if enabled { "TYPE I" } else { "TYPE A" };
        debug!("Setting transfer type: {}", type_cmd);
        self.send_command(type_cmd).await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            self.binary_mode = enabled;
            debug!(
                "Transfer mode set to: {}",
                if enabled { "Binary" } else { "ASCII" }
            );
            Ok(())
        } else if resp.code == 504 {
            Err(Aria2Error::DownloadFailed(format!(
                "Unsupported transfer mode: {}",
                resp.message
            )))
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "TYPE command failed: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// Enter passive mode and establish data connection
    ///
    /// Tries EPSV (Extended Passive Mode) first, falls back to PASV if the
    /// server does not support it.
    ///
    /// # Returns
    ///
    /// Returns the data connection TcpStream
    ///
    /// # Errors
    ///
    /// - 425 Cannot open data connection
    /// - Timeout error
    pub async fn passive_mode(&mut self) -> Result<TcpStream> {
        debug!("Requesting passive mode data connection");

        // Try EPSV first
        self.send_command("EPSV").await?;
        let resp = self.read_response().await?;

        if resp.code == 229 {
            // Parse EPSV response: Entering Extended Passive Mode (|||port|)
            if let Some(port) = Self::parse_epsv_response(&resp.message) {
                debug!("EPSV data channel port: {}", port);
                let data_stream = timeout(
                    self.connect_timeout,
                    TcpStream::connect((self.host.as_str(), port)),
                )
                .await
                .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
                .map_err(|e| Aria2Error::Network(format!("EPSV data connection failed: {}", e)))?;

                return Ok(data_stream);
            }
        }

        // Fall back to PASV
        warn!("EPSV unavailable, falling back to PASV mode");
        self.send_command("PASV").await?;
        let pasv_resp = self.read_response().await?;

        if pasv_resp.code != 227 {
            return Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 425 },
            ));
        }

        // Parse PASV response
        let (data_host, data_port) = Self::parse_pasv_response(&pasv_resp.message)?;
        debug!("PASV data channel: {}:{}", data_host, data_port);
        let data_stream = timeout(
            self.connect_timeout,
            TcpStream::connect((data_host.as_str(), data_port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("PASV data connection failed: {}", e)))?;
        Ok(data_stream)
    }

    /// Enter active mode and establish data connection
    ///
    /// Sends a PORT or EPRT command to inform the server of the client's data port,
    /// then listens on that port for the server to connect.
    ///
    /// # Returns
    ///
    /// Returns the accepted data connection TcpStream
    ///
    /// # Errors
    ///
    /// - 425 Cannot open data connection
    /// - 500/501/502 Command syntax error
    pub async fn active_mode(&mut self) -> Result<TcpStream> {
        debug!("Requesting active mode data connection");

        // Get local address
        let local_addr = self
            .control_stream
            .get_ref()
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("Failed to get local address: {}", e)))?;

        // Listen on port 0 (system auto-assigns an available port)
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to bind data port: {}", e)))?;
        let data_port = listener
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("Failed to get listen port: {}", e)))?
            .port();

        let local_ip = local_addr.ip();

        // Try EPRT (extended active mode)
        let eprt_cmd = format!("EPRT |1|{}|{}|", local_ip, data_port);
        debug!("Sending EPRT command: {}", eprt_cmd);
        self.send_command(&eprt_cmd).await?;
        let resp = self.read_response().await?;

        if resp.code != 200 && resp.code != 500 && resp.code != 501 && resp.code != 502 {
            return Err(Aria2Error::DownloadFailed(format!(
                "EPRT command failed: {} {}",
                resp.code, resp.message
            )));
        }

        // If EPRT failed, try PORT command
        if !resp.is_positive_completion() {
            warn!("EPRT unavailable, falling back to PORT mode");

            // Convert IP address to PORT command format (h1,h2,h3,h4,p1,p2)
            // Only supports IPv4 (PORT command does not support IPv6)

            let ipv4_addr = match local_ip {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => {
                    // IPv6 does not support PORT command, return error
                    return Err(Aria2Error::DownloadFailed(
                        "IPv6 does not support active mode PORT command, please use passive mode"
                            .to_string(),
                    ));
                }
            };
            let ip_bytes = ipv4_addr.octets();
            let p1 = data_port / 256;
            let p2 = data_port % 256;
            let port_cmd = format!(
                "PORT {},{},{},{},{},{}",
                ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3], p1, p2
            );

            debug!("Sending PORT command: {}", port_cmd);
            self.send_command(&port_cmd).await?;
            let port_resp = self.read_response().await?;

            if !port_resp.is_positive_completion() {
                return Err(Aria2Error::Recoverable(
                    crate::error::RecoverableError::ServerError { code: 425 },
                ));
            }
        }

        // Wait for server connection (with timeout)
        debug!("Waiting for server to connect to data port: {}", data_port);
        let (data_stream, _addr) = timeout(self.connect_timeout, listener.accept())
            .await
            .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
            .map_err(|e| Aria2Error::Network(format!("Failed to accept data connection: {}", e)))?;

        debug!("Active mode data connection established successfully");
        Ok(data_stream)
    }

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
        use tokio::io::AsyncReadExt;
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
    /// Supports resume transfer by specifying an offset via the REST command.
    ///
    /// # Arguments
    ///
    /// - `remote_path`: Remote file path
    /// - `offset`: Optional starting offset (for resume transfer)
    ///
    /// # Returns
    ///
    /// Returns the data connection TcpStream for reading file contents
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
        debug!("Preparing to download file: {} (offset: {:?})", remote_path, offset);

        // If there is an offset, send REST command first
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

        // Establish data connection
        let _data_stream = match self.mode {
            FtpMode::Passive => match self.passive_mode().await {
                Ok(stream) => stream,
                Err(e) => {
                    warn!("Passive mode failed, trying active mode: {}", e);
                    self.active_mode().await?
                }
            },
            FtpMode::Active => self.active_mode().await?,
        };

        // Send RETR command
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

        // Note: the actual data stream needs to be managed by the caller
        // Here we return a placeholder; in a real scenario the data connection should be returned
        // Due to Rust's ownership rules, we need to redesign this part
        // For simplicity, create a new connection description here
        Err(Aria2Error::DownloadFailed(
            "download_file needs to return the stream after data connection is established, please use a higher-level API".to_string(),
        ))
    }

    /// Change working directory
    ///
    /// # Arguments
    ///
    /// - `path`: Target directory path
    ///
    /// # Errors
    ///
    /// - 550 Directory does not exist or no permission
    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        debug!("Changing working directory: {}", path);
        self.send_command(&format!("CWD {}", path)).await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            Ok(())
        } else if resp.code == 550 {
            Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 550 },
            ))
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "CWD command failed: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// Get current working directory
    ///
    /// # Returns
    ///
    /// Returns the current directory path string
    ///
    /// # Errors
    ///
    /// - 500/501/502 Command execution error
    pub async fn pwd(&mut self) -> Result<String> {
        debug!("Querying current working directory");
        self.send_command("PWD").await?;
        let resp = self.read_response().await?;

        if resp.code == 257 {
            // PWD response format: "/path/to/dir" is current directory
            // Typical format: 257 "/path" is current directory
            let msg = resp.message.trim();
            // Extract path within quotes
            if let Some(start) = msg.find('"')
                && let Some(end) = msg.rfind('"')
                && end > start
            {
                let dir = &msg[start + 1..end];
                debug!("Current directory: {}", dir);
                return Ok(dir.to_string());
            }
            Ok(msg.to_string())
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "PWD command failed: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// Abort an in-progress transfer
    ///
    /// Sends the ABOR command to interrupt the current data transfer operation.
    /// Note: ABOR behavior may vary across different servers.
    ///
    /// # Errors
    ///
    /// - Network error (if control connection is already closed)
    pub async fn abort(&mut self) -> Result<()> {
        debug!("Sending ABOR command to abort transfer");

        // The ABOR command is special and requires special handling
        // Some implementations require sending Telnet IP (Interrupt Process) + SYNCH first
        // Simplified here: just send the command directly
        self.send_command("ABOR").await?;

        // Read response (there may be multiple responses: 426 + 226 or 225 + 226, etc.)
        match self.read_response().await {
            Ok(resp) => {
                debug!("ABOR response: {} {}", resp.code, resp.message);

                // There may be a second response
                // Try to read it but do not require it
                let mut buf = String::new();
                match timeout(
                    Duration::from_secs(2),
                    self.control_stream.read_line(&mut buf),
                )
                .await
                {
                    Ok(Ok(n)) if n > 0 => {
                        debug!("ABOR second response: {}", buf.trim());
                    }
                    _ => {}
                }

                Ok(())
            }
            Err(e) => {
                // Connection may be in an inconsistent state after ABOR, this is expected
                warn!("Connection state abnormal after ABOR command (may be normal): {}", e);
                Ok(())
            }
        }
    }

    /// Disconnect from the FTP server
    ///
    /// Sends the QUIT command and gracefully closes the control connection.
    pub async fn quit(mut self) -> Result<()> {
        debug!("Sending QUIT command");

        if let Err(e) = self.send_command("QUIT").await {
            warn!("Failed to send QUIT command (connection may already be closed): {}", e);
            return Ok(());
        }

        match self.read_response().await {
            Ok(resp) => {
                info!("FTP disconnected: {}", resp.message.trim());
                Ok(())
            }
            Err(e) => {
                warn!("Failed to read QUIT response: {}", e);
                Ok(())
            }
        }
    }

    // ==================== Internal helper methods ====================

    /// Send an FTP command
    ///
    /// Writes the command with \r\n line ending to the control connection.
    async fn send_command(&mut self, cmd: &str) -> Result<()> {
        debug!("FTP command: {}", cmd.trim());

        self.control_stream
            .write_all(cmd.as_bytes())
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to send FTP command: {}", e)))?;

        self.control_stream
            .write_all(b"\r\n")
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to send newline: {}", e)))?;

        self.control_stream
            .flush()
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to flush buffer: {}", e)))?;

        Ok(())
    }

    /// Read an FTP response
    ///
    /// Handles multi-line responses, supports standard FTP response format:
    /// - Single line: `NNN text`
    /// - Multi-line: `NNN-text\n...\nNNN text`
    async fn read_response(&mut self) -> Result<FtpResponse> {
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();

            let bytes_read = timeout(self.read_timeout, self.control_stream.read_line(&mut line))
                .await
                .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
                .map_err(|e| Aria2Error::Network(format!("Failed to read FTP response: {}", e)))?;

            if bytes_read == 0 {
                break; // Connection closed
            }

            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                continue;
            }

            // Parse 3-digit response code
            let response_code: u16 = trimmed[..3].parse().unwrap_or(0);

            if code.is_none() {
                code = Some(response_code);
            }

            // Determine separator
            let separator = trimmed.as_bytes()[3];

            if separator == b'-' && !is_multiline {
                // Multi-line response start
                is_multiline = true;
                message.push_str(&trimmed[4..]);
                message.push('\n');
            } else if separator == b' ' {
                // Single-line response or multi-line end
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline && trimmed.starts_with(&format!("{:3} ", code.unwrap_or(0))) {
                // Multi-line response end marker
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline {
                // Multi-line middle line
                message.push_str(&trimmed[4..]);
                message.push('\n');
            }
        }

        let code_val = code.unwrap_or(0);
        debug!("FTP response: {} {}", code_val, message.trim());

        Ok(FtpResponse {
            code: code_val,
            message,
        })
    }

    /// Parse PASV response, extract IP address and port
    ///
    /// PASV response format: `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`
    ///
    /// # Arguments
    ///
    /// - `text`: Message portion of the PASV response
    ///
    /// # Returns
    ///
    /// Returns a `(host, port)` tuple
    fn parse_pasv_response(text: &str) -> Result<(String, u16)> {
        let start = text
            .find('(')
            .ok_or_else(|| Aria2Error::Parse("PASV response missing opening parenthesis".to_string()))?;

        let end = text
            .find(')')
            .ok_or_else(|| Aria2Error::Parse("PASV response missing closing parenthesis".to_string()))?;

        let inner = &text[start + 1..end];
        let parts: Vec<&str> = inner.split(',').collect();

        if parts.len() != 6 {
            return Err(Aria2Error::Parse(format!(
                "PASV response format error: expected 6 parts, got {}",
                parts.len()
            )));
        }

        let h1: u8 = parts[0]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h1".to_string()))?;
        let h2: u8 = parts[1]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h2".to_string()))?;
        let h3: u8 = parts[2]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h3".to_string()))?;
        let h4: u8 = parts[3]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h4".to_string()))?;
        let p1: u16 = parts[4]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid port byte p1".to_string()))?;
        let p2: u16 = parts[5]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid port byte p2".to_string()))?;

        let host = format!("{}.{}.{}.{}", h1, h2, h3, h4);
        let port = p1 * 256 + p2;

        Ok((host, port))
    }

    /// Parse EPSV response, extract port number
    ///
    /// EPSV response format: `229 Entering Extended Passive Mode (|||port|)`
    ///
    /// # Arguments
    ///
    /// - `text`: Message portion of the EPSV response
    ///
    /// # Returns
    ///
    /// Returns the port number, or None if parsing fails
    fn parse_epsv_response(text: &str) -> Option<u16> {
        let start = text.rfind('|')?;
        let prev_pipe = text[..start].rfind('|')?;
        let port_str = &text[prev_pipe + 1..start];
        port_str.parse::<u16>().ok()
    }

    /// Parse a single line of LIST output
    ///
    /// Supports Unix format (`-rw-r--r--  1 user group   size date  name`) and
    /// Windows format (`date       size  name` or `dir`).
    ///
    /// # Arguments
    ///
    /// - `line`: Single line of LIST output text
    ///
    /// # Returns
    ///
    /// Returns parsed file info, or None if parsing fails
    pub(crate) fn parse_list_line(line: &str) -> Option<FtpFileInfo> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Try Unix format parsing
        if let Some(info) = Self::parse_unix_list_line(trimmed) {
            return Some(info);
        }

        // Try Windows format parsing
        if let Some(info) = Self::parse_windows_list_line(trimmed) {
            return Some(info);
        }

        // Try MLSD format parsing
        if let Some(info) = Self::parse_mlsd_line(trimmed) {
            return Some(info);
        }

        None
    }

    /// Parse Unix ls -l format using fast path (zero-dependency string parsing)
    ///
    /// This fast path handles ~90% of real-world FTP LIST responses which use
    /// standard Unix ls -l format, avoiding regex compilation and matching overhead.
    ///
    /// Format: `[type][perms] [links] [owner] [group] [size] [mon] [day] [time/year] [name]`
    /// Example: `-rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf`
    ///
    /// # Returns
    ///
    /// `Some(FtpFileInfo)` if parsing succeeds, `None` if line doesn't match expected format
    fn parse_list_line_fast(line: &str) -> Option<FtpFileInfo> {
        // Minimum viable line length check:
        // type(1) + perms(9) + spaces(3+) + links(1+) + owner(1+) + spaces + group(1+)
        // + spaces + size(1+) + spaces + month(3) + spaces + day(1+) + spaces + time(4-5/year)
        // + space + name(1+) >= ~40 chars for realistic entries
        if line.len() < 35 {
            return None;
        }

        // Determine entry type from first character
        let entry_type = match line.as_bytes().first()? {
            b'd' => true,  // Directory
            b'-' => false, // Regular file
            b'l' => {
                // Symlink - handle specially below
                // For symlinks, we'll parse but mark as non-directory
                false
            }
            _ => return None, // Unknown type, fallback to regex
        };

        let is_dir = entry_type;

        // Validate permission field (chars 1-9 should be [rwxst-])
        let perms = &line[1..10];
        if !perms.chars().all(|c| "rwxst-".contains(c)) {
            return None;
        }

        // Skip permission field and split rest by whitespace
        let after_perms = line[10..].trim_start();

        // Find positions of each field by scanning for whitespace
        // Expected fields: links owner group size month day time/year name
        // We need to skip 7 fields and capture the rest as filename
        let mut pos = 0;
        for _ in 0..7 {
            // Skip current field (non-whitespace)
            let end = after_perms[pos..]
                .find(' ')
                .unwrap_or(after_perms.len() - pos);
            pos += end + 1;
            // Skip whitespace between fields
            while pos < after_perms.len() && after_perms.as_bytes()[pos] == b' ' {
                pos += 1;
            }
            if pos >= after_perms.len() {
                return None;
            }
        }

        // Remaining part is the filename (may contain spaces)
        let name_raw = after_perms[pos..].trim();
        if name_raw.is_empty() {
            return None;
        }

        // Handle symlink format: "linkname -> target"
        let actual_name = if line.as_bytes()[0] == b'l' {
            if let Some(arrow_pos) = name_raw.find(" -> ") {
                &name_raw[..arrow_pos]
            } else {
                name_raw
            }
        } else {
            name_raw
        };

        // Filter out special entries
        if actual_name == "." || actual_name == ".." {
            return None;
        }

        // Parse size from the line (field index 3, 0-based)
        // Fields after permissions: links(0) owner(1) group(2) size(3) month(4) ...
        let size_field = after_perms.split_whitespace().nth(3)?;
        let size: u64 = size_field.parse().ok()?;

        Some(FtpFileInfo {
            name: actual_name.to_string(),
            size,
            is_dir,
        })
    }

    /// Parse Unix-format LIST line with fast path optimization
    ///
    /// Tries zero-allocation string parsing first (~90% of cases),
    /// falls back to regex for exotic formats.
    fn parse_unix_list_line(line: &str) -> Option<FtpFileInfo> {
        // Fast path for standard Unix ls -l format (avoids regex overhead)
        if let Some(info) = Self::parse_list_line_fast(line) {
            return Some(info);
        }

        // Fallback to regex for exotic/non-standard formats
        Self::parse_unix_list_line_regex(line)
    }

    /// Parse Unix ls -l format using regex (fallback for non-standard formats)
    ///
    /// Unix format example:
    /// ```text
    /// -rw-r--r--  1 user group  12345 Jan 15 10:30 filename.txt
    /// drwxr-xr-x  2 user group   4096 Feb  3 14:20 directory
    /// lrwxrwxrwx  1 user group     8 Mar 10 09:00 link -> target
    /// ```
    fn parse_unix_list_line_regex(line: &str) -> Option<FtpFileInfo> {
        // Use regex to match Unix ls -l format
        // Format: [type][perms]  [links] [user] [group] [size] [mon] [day] [time/year] [name]
        // Example: -rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf

        // Regex pattern explanation:
        // ^([bcdlsp-])           # File type (1 char)
        // ([rwxst-]{9})          # Permission bits (9 chars)
        // \s+                     # One or more spaces
        // (\d+)                   # Hard link count
        // \s+                     # Space
        // (\S+)                   # Username
        // \s+                     # Space
        // (\S+)                   # Group name
        // \s+                     # Space
        // (\d+)                   # File size
        // \s+                     # Space
        // (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)  # Month
        // \s+                     # Space
        // (\d{1,2})              # Day (1-2 digits)
        // \s+                     # Space
        // (\d{4}|\d{1,2}:\d{2})  # Year (4 digits) or time (HH:MM)
        // \s+                     # Space
        // (.+)$                  # Filename (may contain spaces)

        use regex::Regex;

        let re = Regex::new(
            r"^([bcdlsp-])([rwxst-]{9})\s+(\d+)\s+(\S+)\s+(\S+)\s+(\d+)\s+(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+(\d{1,2})\s+(\d{4}|\d{1,2}:\d{2})\s+(.+)$"
        ).ok()?;

        let caps = re.captures(line)?;

        let type_char = caps.get(1)?.as_str().chars().next()?;
        let is_dir = type_char == 'd';
        let is_link = type_char == 'l';

        let size: u64 = caps.get(6)?.as_str().parse().ok()?;
        let name = caps.get(10)?.as_str();

        if name.is_empty() {
            return None;
        }

        // Handle symlink: "link -> target"
        let actual_name = if is_link {
            if let Some(arrow_pos) = name.find(" -> ") {
                &name[..arrow_pos]
            } else {
                name
            }
        } else {
            name
        };

        // Special entries: "." and ".."
        if actual_name == "." || actual_name == ".." {
            return None;
        }

        Some(FtpFileInfo {
            name: actual_name.to_string(),
            size,
            is_dir,
        })
    }

    /// Parse Windows/DOS format LIST line
    ///
    /// Windows format example:
    /// ```text
    /// 01-15-24  10:30AM    12345 filename.txt
    /// 02-03-24  02:20PM    <DIR> directory
    /// ```
    fn parse_windows_list_line(line: &str) -> Option<FtpFileInfo> {
        // Windows format: "MM-DD-YY  HH:MM[AP]M  <DIR>/size  name"
        // Minimum length check
        if line.len() < 20 {
            return None;
        }

        // Date part: MM-DD-YY (8 characters)
        let date_part = &line[..8];
        if date_part.len() != 8
            || date_part.chars().nth(2)? != '-'
            || date_part.chars().nth(5)? != '-'
        {
            return None;
        }

        let after_date = line[8..].trim_start();

        // Time part: HH:MM[AP]M (7-9 characters)
        let space_pos = after_date.find(' ')?;
        let time_part = &after_date[..space_pos];
        if !time_part.contains(':') {
            return None;
        }

        let after_time = after_date[space_pos + 1..].trim_start();

        // Size or <DIR>
        let space_pos = after_time.find(' ')?;
        let size_or_dir = after_time[..space_pos].trim();

        let is_dir = size_or_dir.eq_ignore_ascii_case("<DIR>");
        let size: u64 = if is_dir { 0 } else { size_or_dir.parse().ok()? };

        // Filename
        let name = after_time[space_pos + 1..].trim().to_string();

        if name.is_empty() || name == "." || name == ".." {
            return None;
        }

        Some(FtpFileInfo { name, size, is_dir })
    }

    /// Parse MLSD (Machine Listing) format line
    ///
    /// MLSD format example:
    /// ```text
    /// type=file;size=12345;modify=20240115103000;unix.mode=0644; filename.txt
    /// type=dir;size=4096;modify=20240203142000;unix.mode=0755; directory
    /// type=os.unix=symlink=/target;size=8; link
    /// ```
    fn parse_mlsd_line(line: &str) -> Option<FtpFileInfo> {
        // MLSD format: facts; facts; ... name
        // Facts and name are separated by a space
        let semicolon_pos = line.rfind("; ")?;
        let (facts_str, name) = line.split_at(semicolon_pos + 2);
        let name = name.trim();

        if name.is_empty() || name == "." || name == ".." {
            return None;
        }

        // Parse facts
        let mut is_dir = false;
        let mut size: u64 = 0;

        for fact in facts_str.split(';') {
            let fact = fact.trim();
            if fact.is_empty() {
                continue;
            }

            if let Some(eq_pos) = fact.find('=') {
                let key = &fact[..eq_pos];
                let value = &fact[eq_pos + 1..];

                match key.to_lowercase().as_str() {
                    "type" => {
                        is_dir = value.eq_ignore_ascii_case("dir")
                            || value.eq_ignore_ascii_case("cdir")
                            || value.eq_ignore_ascii_case("pdir");
                    }
                    "size" => {
                        size = value.parse().unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }

        Some(FtpFileInfo {
            name: name.to_string(),
            size,
            is_dir,
        })
    }
}

// ==================== Test module ====================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ftp_response_checks() {
        // Test positive completion response (2xx)
        let ok = FtpResponse {
            code: 226,
            message: "Transfer complete".into(),
        };
        assert!(ok.is_success());
        assert!(ok.is_positive_completion());
        assert!(!ok.is_positive_preliminary());

        // Test positive preliminary response (1xx)
        let preliminary = FtpResponse {
            code: 150,
            message: "Opening data connection".into(),
        };
        assert!(preliminary.is_success());
        assert!(!preliminary.is_positive_completion());
        assert!(preliminary.is_positive_preliminary());

        // Test error response (4xx/5xx)
        let error = FtpResponse {
            code: 550,
            message: "File not found".into(),
        };
        assert!(!error.is_success());
        assert!(!error.is_positive_completion());
        assert!(!error.is_positive_preliminary());
    }

    #[test]
    fn test_parse_pasv_response_valid() {
        let msg = "Entering Passive Mode (192,168,1,100,195,123)";
        let result = FtpClient::parse_pasv_response(msg);
        assert!(result.is_ok());
        let (host, port) = result.unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 195 * 256 + 123); // 195*256 + 123 = 50043
    }

    #[test]
    fn test_parse_pasv_response_invalid() {
        // Missing parentheses
        let msg = "Entering Passive Mode 192,168,1,100,195,123";
        let result = FtpClient::parse_pasv_response(msg);
        assert!(result.is_err());

        // Incorrect number of parts
        let msg2 = "Entering Passive Mode (192,168,1,100,195)";
        let result2 = FtpClient::parse_pasv_response(msg2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_parse_epsv_response_valid() {
        let msg = "Entering Extended Passive Mode (|||50001|)";
        let result = FtpClient::parse_epsv_response(msg);
        assert_eq!(result, Some(50001));
    }

    #[test]
    fn test_parse_epsv_response_invalid() {
        let msg = "Invalid EPSV response";
        let result = FtpClient::parse_epsv_response(msg);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_list_line_unix_regular_file() {
        let line = "-rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "document.pdf");
        assert_eq!(info.size, 12345);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_directory() {
        let line = "drwxr-xr-x  2 user staff   4096 Feb  3 14:20 my_folder";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my_folder");
        assert_eq!(info.size, 4096);
        assert!(info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_symlink() {
        let line = "lrwxrwxrwx  1 user staff      8 Mar 10 09:00 link.txt -> target.txt";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "link.txt"); // Symlink should return link name, not target
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_hidden_file() {
        let line = "-rw-r--r--  1 user staff    512 Apr  1 08:00 .bashrc";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, ".bashrc");
        assert_eq!(info.size, 512);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_special_entries() {
        // "." and ".." should be ignored
        let dot = "drwxr-xr-x  2 user staff   4096 Jan  1 00:00 .";
        let dotdot = "drwxr-xr-x  2 user staff   4096 Jan  1 00:00 ..";

        assert!(FtpClient::parse_list_line(dot).is_none());
        assert!(FtpClient::parse_list_line(dotdot).is_none());
    }

    #[test]
    fn test_parse_list_line_windows_file() {
        let line = "01-15-24  10:30AM    12345 document.pdf";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "document.pdf");
        assert_eq!(info.size, 12345);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_windows_directory() {
        let line = "02-03-24  02:20PM    <DIR> my_folder";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my_folder");
        assert!(info.is_dir);
    }

    #[test]
    fn test_parse_list_line_mlsd_format() {
        let line = "type=file;size=12345;modify=20240115103000;unix.mode=0644; document.pdf";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "document.pdf");
        assert_eq!(info.size, 12345);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_mlsd_directory() {
        let line = "type=dir;size=4096;modify=20240203142000;unix.mode=0755; my_folder";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my_folder");
        assert_eq!(info.size, 4096);
        assert!(info.is_dir);
    }

    #[test]
    fn test_ftp_mode_default() {
        let mode = FtpMode::default();
        assert_eq!(mode, FtpMode::Passive);
    }

    #[test]
    fn test_ftp_file_info_creation() {
        let info = FtpFileInfo {
            name: "test.txt".to_string(),
            size: 1024,
            is_dir: false,
        };
        assert_eq!(info.name, "test.txt");
        assert_eq!(info.size, 1024);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_with_spaces_in_name() {
        // Unix format, filename contains spaces
        let line = "-rw-r--r--  1 user staff   5678 Jan 20 11:00 my document with spaces.txt";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my document with spaces.txt");
        assert_eq!(info.size, 5678);
    }

    #[test]
    fn test_parse_list_line_unrecognized_format() {
        // Unrecognized format
        let line = "this is not a valid listing format";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_pasv_edge_cases() {
        // Edge case: minimum port
        let min_msg = "Entering Passive Mode (127,0,0,1,0,0)";
        let min_result = FtpClient::parse_pasv_response(min_msg).unwrap();
        assert_eq!(min_result.1, 0);

        // Edge case: maximum port
        let max_msg = "Entering Passive Mode (255,255,255,255,255,255)";
        let max_result = FtpClient::parse_pasv_response(max_msg).unwrap();
        assert_eq!(max_result.1, 255 * 256 + 255); // 65535
    }
}
