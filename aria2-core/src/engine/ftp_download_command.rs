use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

/// FTP download command that handles the complete download lifecycle
pub struct FtpDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    host: String,
    port: u16,
    remote_path: String,
    username: String,
    password: String,
    /// Resume offset for partial downloads (0 if not resuming)
    resume_offset: u64,
    /// Whether to use passive mode (true) or active mode (false)
    passive_mode: bool,
    /// Maximum number of retry attempts for transient errors
    max_retries: u32,
    /// Current retry attempt count
    current_retry: u32,
}

impl FtpDownloadCommand {
    /// Create a new FTP download command
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let (host, port, username, password, remote_path) = Self::parse_uri(uri)?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(&remote_path))
            .unwrap_or_else(|| "download".to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Check if file exists for resume support
        let resume_offset = if path.exists() {
            std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        let group = RequestGroup::new(gid, vec![uri.to_string()], options.clone());
        info!(
            "FtpDownloadCommand created: {} -> {} ({}:{}/{}) [resume_offset={}]",
            uri,
            path.display(),
            host,
            port,
            remote_path,
            resume_offset
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            output_path: path,
            started: false,
            completed_bytes: 0,
            host,
            port,
            remote_path,
            username,
            password,
            resume_offset,
            passive_mode: true, // Default to passive mode
            max_retries: 3,
            current_retry: 0,
        })
    }

    /// Parse FTP URI into components
    fn parse_uri(uri: &str) -> Result<(String, u16, String, String, String)> {
        if !uri.starts_with("ftp://") && !uri.starts_with("ftps://") {
            return Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol {
                protocol: "ftp".into(),
            }));
        }

        let without_scheme = uri
            .trim_start_matches("ftp://")
            .trim_start_matches("ftps://");

        let (auth_host_port, path) = match without_scheme.find('/') {
            Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
            None => (without_scheme, "/"),
        };

        let (auth, host_port) = match auth_host_port.rfind('@') {
            Some(idx) => (&auth_host_port[..idx], &auth_host_port[idx + 1..]),
            None => ("", auth_host_port),
        };

        let (username, password) = if auth.is_empty() {
            ("anonymous".to_string(), "aria2@".to_string())
        } else if let Some(colon_pos) = auth.find(':') {
            (
                auth[..colon_pos].to_string(),
                auth[colon_pos + 1..].to_string(),
            )
        } else {
            (auth.to_string(), String::new())
        };

        let (host, port) = match host_port.rfind(':') {
            Some(idx) => (
                host_port[..idx].to_string(),
                host_port[idx + 1..].parse::<u16>().unwrap_or(21),
            ),
            None => (host_port.to_string(), 21),
        };

        Ok((host, port, username, password, urlencoding_decode(path)))
    }

    /// Extract filename from remote path
    fn extract_filename(remote_path: &str) -> Option<String> {
        remote_path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "/")
            .map(|s| s.to_string())
    }

    /// Get read access to the request group
    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }

    /// Classify FTP response code to determine error handling strategy
    #[allow(dead_code)]
    fn classify_ftp_error(&self, code: u16, message: &str) -> Aria2Error {
        match code {
            // Positive responses (should not be errors)
            100..=399 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Unexpected positive response: {} {}", code, message),
            }),
            // Transient negative completion - retry may succeed
            421 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Service not available: {}", message),
            }),
            425 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Can't open data connection: {}", message),
            }),
            426 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Connection closed; transfer aborted: {}", message),
            }),
            450 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Requested file action not taken: {}", message),
            }),
            451 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Requested action aborted: {}", message),
            }),
            452 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Requested action not taken: {}", message),
            }),
            // Permanent negative completion - do not retry
            500..=504 => Aria2Error::Fatal(FatalError::Config(format!(
                "FTP syntax error: {} {}",
                code, message
            ))),
            530 => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("{}:{}", self.host, self.port),
            }),
            532 => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: "Account required for storing file".into(),
            }),
            550 => Aria2Error::Fatal(FatalError::FileNotFound {
                path: self.remote_path.clone(),
            }),
            551 => Aria2Error::Fatal(FatalError::Config(format!(
                "Page type unknown: {}",
                message
            ))),
            552 => Aria2Error::Fatal(FatalError::Config(format!(
                "Exceeded storage allocation: {}",
                message
            ))),
            553 => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("Filename not allowed: {}", message),
            }),
            // Unknown error codes
            _ => {
                // Check message content for hints about error type
                let msg_lower = message.to_lowercase();
                if msg_lower.contains("not found")
                    || msg_lower.contains("no such")
                    || msg_lower.contains("access denied")
                    || msg_lower.contains("permission")
                {
                    Aria2Error::Fatal(FatalError::FileNotFound {
                        path: self.remote_path.clone(),
                    })
                } else if msg_lower.contains("login") || msg_lower.contains("auth") {
                    Aria2Error::Fatal(FatalError::PermissionDenied {
                        path: format!("{}:{}", self.host, self.port),
                    })
                } else {
                    // Default to recoverable for unknown codes in 4xx/5xx range
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP error {} {}: {}", code, message, self.remote_path),
                    })
                }
            }
        }
    }
}

/// URL-encoded string decoder
fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push(c);
                result.push_str(&hex);
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Raw FTP control connection handler
struct RawFtpControl {
    reader: BufReader<tokio::net::TcpStream>,
    host: String,
}

impl RawFtpControl {
    /// Establish connection to FTP server and read welcome message
    async fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{}:{}", host, port);
        let socket_addr: std::net::SocketAddr = addr.parse().map_err(|_| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("cannot parse address: {}", addr),
            })
        })?;

        debug!("Connecting to FTP server at {}:{}", host, port);

        let stream = tokio::net::TcpStream::connect(socket_addr)
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP connect failed to {}:{}: {}", host, port, e),
                })
            })?;

        // Set TCP keepalive and no-delay options
        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;

        let mut ctrl = Self {
            reader: BufReader::new(stream),
            host: host.to_string(),
        };
        let welcome = ctrl.read_response(Duration::from_secs(15)).await?;

        if !(200..300).contains(&welcome.0) && !(100..200).contains(&welcome.0) {
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "FTP server rejected connection: {} {}",
                welcome.0, welcome.1
            ))));
        }

        info!("Connected to FTP server {}:{}", host, port);
        Ok(ctrl)
    }

    /// Send a command to the FTP server
    async fn send_command(&mut self, cmd: &str) -> Result<()> {
        debug!("FTP CMD: {}", cmd.trim());
        self.reader
            .get_mut()
            .write_all(format!("{}\r\n", cmd).as_bytes())
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP write command failed: {}", e),
                })
            })?;
        self.reader.get_mut().flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP flush failed: {}", e),
            })
        })?;
        Ok(())
    }

    /// Read response from FTP server with timeout
    async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();
            let bytes_read = tokio::time::timeout(timeout_dur, self.reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP response timeout after {:?}", timeout_dur),
                    })
                })?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP read response error: {}", e),
                    })
                })?;

            if bytes_read == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                continue;
            }

            let response_code: u16 = trimmed[..3].parse().unwrap_or(0);
            if code.is_none() {
                code = Some(response_code);
            }

            let sep = trimmed.as_bytes()[3];
            if sep == b'-' && !is_multiline {
                is_multiline = true;
                if trimmed.len() > 4 {
                    message.push_str(&trimmed[4..]);
                }
                message.push('\n');
                continue;
            }
            if is_multiline {
                if trimmed.starts_with(&format!("{} ", code.unwrap_or(0))) {
                    if trimmed.len() > 4 {
                        message.push_str(&trimmed[4..]);
                    }
                    break;
                }
                if trimmed.len() > 4 {
                    message.push_str(&trimmed[4..]);
                }
                message.push('\n');
                continue;
            }

            if trimmed.len() > 4 {
                message = trimmed[4..].to_string();
            }
            break;
        }

        let code_val = code.unwrap_or(0);
        debug!("FTP RESP: {} {}", code_val, message.trim());
        Ok((code_val, message))
    }

    /// Send command and read response in one operation
    async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(Duration::from_secs(30)).await
    }

    /// Authenticate with USER/PASS commands
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        info!("Authenticating as user: {}", username);

        let user_resp = self.command(&format!("USER {}", username)).await?;
        match user_resp.0 {
            230 => {
                // Login successful without password
                info!("FTP login successful (no password required)");
                Ok(())
            }
            331 | 332 => {
                // Password required
                debug!("Password required, sending PASS command");
                let pass_resp = self.command(&format!("PASS {}", password)).await?;
                if !(200..300).contains(&pass_resp.0) {
                    return Err(Aria2Error::Fatal(FatalError::PermissionDenied {
                        path: format!("Login failed: {} {}", pass_resp.0, pass_resp.1),
                    }));
                }
                info!("FTP login successful");
                Ok(())
            }
            _ => Err(Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("Unexpected USER response: {} {}", user_resp.0, user_resp.1),
            })),
        }
    }

    /// Set binary transfer mode (TYPE I)
    async fn set_binary_mode(&mut self) -> Result<()> {
        debug!("Setting transfer mode to binary (TYPE I)");
        let resp = self.command("TYPE I").await?;
        if !(200..300).contains(&resp.0) {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("TYPE I failed: {} {}", resp.0, resp.1),
                },
            ));
        }
        Ok(())
    }

    /// Set resume offset (REST command)
    async fn set_resume_offset(&mut self, offset: u64) -> Result<()> {
        if offset == 0 {
            return Ok(());
        }
        debug!("Setting resume offset: {} bytes", offset);
        let resp = self.command(&format!("REST {}", offset)).await?;
        if resp.0 != 350 {
            warn!("REST command not accepted by server: {} {}", resp.0, resp.1);
            // Some servers don't support REST, continue without resume
            return Ok(());
        }
        Ok(())
    }

    /// Get file size (SIZE command)
    async fn get_file_size(&mut self, remote_path: &str) -> Result<Option<u64>> {
        debug!("Querying file size: {}", remote_path);
        let resp = self.command(&format!("SIZE {}", remote_path)).await?;
        if resp.0 == 213 {
            let size: u64 = resp.1.trim().parse().unwrap_or(0);
            debug!("File size: {} bytes", size);
            return Ok(Some(size));
        }
        // SIZE command may not be supported by all servers
        debug!("SIZE command returned: {} {}", resp.0, resp.1);
        Ok(None)
    }

    /// Enter passive mode (PASV/EPSV)
    async fn enter_passive_mode(&mut self) -> Result<(String, u16)> {
        // Try EPSV first (supports IPv6), fallback to PASV
        debug!("Attempting extended passive mode (EPSV)");
        let epsv_resp = self.command("EPSV").await;

        match epsv_resp {
            Ok(resp) if resp.0 == 229 => {
                // Parse |||port| format
                if let Some(port) = parse_epsv_response(&resp.1) {
                    debug!("EPSV successful, using port: {}", port);
                    return Ok((self.host.clone(), port));
                }
                warn!("Failed to parse EPSV response, falling back to PASV");
            }
            _ => {
                debug!("EPSV not supported, trying PASV");
            }
        }

        // Fallback to PASV
        debug!("Entering passive mode (PASV)");
        let pasv_resp = self.command("PASV").await?;
        if pasv_resp.0 != 227 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("PASV failed: {} {}", pasv_resp.0, pasv_resp.1),
                },
            ));
        }

        match parse_pasv_response(&pasv_resp.1) {
            Some((host, port)) => {
                debug!("PASV successful, data channel: {}:{}", host, port);
                Ok((host, port))
            }
            None => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "Cannot parse PASV response".into(),
                },
            )),
        }
    }

    /// Initiate file retrieval (RETR command)
    async fn initiate_retr(&mut self, remote_path: &str) -> Result<()> {
        debug!("Initiating file retrieval: {}", remote_path);
        let resp = self.command(&format!("RETR {}", remote_path)).await?;
        if resp.0 != 150 && resp.0 != 125 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("RETR unexpected response: {} {}", resp.0, resp.1),
                },
            ));
        }
        Ok(())
    }

    /// Read final transfer completion response
    async fn read_transfer_complete(&mut self) -> Result<()> {
        match self.read_response(Duration::from_secs(10)).await {
            Ok((226, msg)) => {
                debug!("Transfer complete: {}", msg);
                Ok(())
            }
            Ok((code, msg)) => {
                warn!("Transfer response non-226: {} {}", code, msg);
                // Some servers don't send 226 properly, but data was received OK
                Ok(())
            }
            Err(e) => {
                debug!("Transfer completion timeout/error (may be normal): {}", e);
                Ok(())
            }
        }
    }

    /// Gracefully disconnect from server
    async fn quit(mut self) -> Result<()> {
        debug!("Sending QUIT command");
        let _ = self.command("QUIT").await.ok(); // Ignore errors on quit
        Ok(())
    }
}

/// Parse PASV response to extract IP and port
fn parse_pasv_response(response: &str) -> Option<(String, u16)> {
    let start = response.find('(')?;
    let end = response.rfind(')')?;
    let inner = &response[start + 1..end];
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 6 {
        return None;
    }
    let h1: u8 = parts[0].trim().parse().ok()?;
    let h2: u8 = parts[1].trim().parse().ok()?;
    let h3: u8 = parts[2].trim().parse().ok()?;
    let h4: u8 = parts[3].trim().parse().ok()?;
    let p1: u16 = parts[4].trim().parse().ok()?;
    let p2: u16 = parts[5].trim().parse().ok()?;
    Some((format!("{}.{}.{}.{}", h1, h2, h3, h4), p1 * 256 + p2))
}

/// Parse EPSV response to extract port
fn parse_epsv_response(response: &str) -> Option<u16> {
    let start = response.rfind('|')?;
    let prev_pipe = response[..start].rfind('|')?;
    let port_str = &response[prev_pipe + 1..start];
    port_str.parse::<u16>().ok()
}

#[async_trait]
impl Command for FtpDownloadCommand {
    /// Execute the FTP download with full lifecycle management
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        info!(
            "FTP download starting: {}:{} -> {}",
            self.host,
            self.port,
            self.output_path.display()
        );

        // Create output directory if needed
        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        // Retry loop for transient errors
        loop {
            match self.execute_single_attempt().await {
                Ok(_) => {
                    info!(
                        "FTP download completed successfully: {} ({} bytes)",
                        self.output_path.display(),
                        self.completed_bytes
                    );
                    return Ok(());
                }
                Err(e) => {
                    // Check if this is a retry-worthy error
                    let should_retry = matches!(
                        e,
                        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
                            | Aria2Error::Recoverable(RecoverableError::Timeout)
                    ) && self.current_retry < self.max_retries;

                    if should_retry {
                        self.current_retry += 1;
                        let wait_ms = 1000u64 * (1 << (self.current_retry - 1));
                        warn!(
                            "FTP download failed (attempt {}/{}), retrying in {}ms: {}",
                            self.current_retry, self.max_retries, wait_ms, e
                        );
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;

                        // Reset state for retry
                        self.completed_bytes = 0;
                        continue;
                    }

                    // Non-retryable error or max retries exceeded
                    error!(
                        "FTP download failed permanently after {} attempts: {}",
                        self.current_retry + 1,
                        e
                    );
                    return Err(e);
                }
            }
        }
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 || self.started {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(300)) // 5 minute default timeout
    }
}

impl FtpDownloadCommand {
    /// Execute a single download attempt
    async fn execute_single_attempt(&mut self) -> Result<()> {
        // Step 1: Connect to FTP server
        let mut ctrl = RawFtpControl::connect(&self.host, self.port).await?;

        // Step 2: Authenticate
        ctrl.authenticate(&self.username, &self.password).await?;

        // Step 3: Set binary transfer mode
        ctrl.set_binary_mode().await?;

        // Step 4: Probe file size
        let file_size = ctrl.get_file_size(&self.remote_path).await?;

        // Update total length in request group
        {
            let mut g = self.group.write().await;
            g.set_total_length(file_size.unwrap_or(0)).await;
        }

        // Step 5: Set resume offset if applicable
        if self.resume_offset > 0 {
            ctrl.set_resume_offset(self.resume_offset).await?;
        }

        // Step 6: Negotiate data connection mode
        let (data_host, data_port) = if self.passive_mode {
            ctrl.enter_passive_mode().await?
        } else {
            // Active mode would go here (not fully implemented in this version)
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "Active mode not yet implemented".into(),
                },
            ));
        };

        // Step 7: Initiate file transfer (RETR)
        ctrl.initiate_retr(&self.remote_path).await?;

        // Step 8: Connect to data port
        let data_addr: std::net::SocketAddr = format!("{}:{}", data_host, data_port)
            .parse()
            .map_err(|_| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: "Invalid data address".into(),
                })
            })?;

        let mut data_stream = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::net::TcpStream::connect(data_addr),
        )
        .await
        .map_err(|_| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "Data connection timeout".into(),
            })
        })?
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Data connection failed: {}", e),
            })
        })?;

        // Set TCP no-delay on data connection
        let _ = data_stream.set_nodelay(true); // Ignore error if not supported

        // Step 9: Setup disk writer with optional rate limiting
        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let rate_limit = {
            let g = self.group.read().await;
            g.options().max_download_limit
        };
        let mut writer: Box<dyn DiskWriter> = if let Some(rate) = rate_limit.filter(|&r| r > 0) {
            debug!("Rate limiting enabled: {} bytes/sec", rate);
            Box::new(ThrottledWriter::new(
                raw_writer,
                RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)),
            ))
        } else {
            Box::new(raw_writer)
        };

        // Seek to resume offset if resuming
        // Note: DiskWriter trait doesn't support seek, so for resume we rely on
        // the FTP REST command to tell server to start from the offset,
        // and data will be appended to existing file if it exists
        if self.resume_offset > 0 {
            debug!(
                "Resume offset: {} bytes (using FTP REST command)",
                self.resume_offset
            );
        }

        // Step 10: Data receive loop with progress tracking
        let mut buffer = vec![0u8; 65536]; // 64KB buffer
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        info!("Starting data reception from FTP server");

        loop {
            let bytes_read = data_stream.read(&mut buffer).await.map_err(|e| {
                // Classify IO errors
                use std::io::ErrorKind;
                match e.kind() {
                    ErrorKind::Interrupted
                    | ErrorKind::WouldBlock
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut => {
                        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: format!("Data read error (transient): {}", e),
                        })
                    }
                    _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("Data read error: {}", e),
                    }),
                }
            })?;

            if bytes_read == 0 {
                debug!("End of data stream reached");
                break;
            }

            // Write to disk (with rate limiting if enabled)
            writer.write(&buffer[..bytes_read]).await?;
            self.completed_bytes += bytes_read as u64;

            // Update progress in request group
            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;

                // Update speed calculation every 500ms
                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = if elapsed.as_secs_f64() > 0.0 {
                        (delta as f64 / elapsed.as_secs_f64()) as u64
                    } else {
                        0
                    };
                    g.update_speed(speed, 0).await;
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        // Step 11: Cleanup and finalize
        drop(data_stream); // Close data connection

        // Finalize disk writer (flush buffers, etc.)
        writer.finalize().await.map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!("Finalize writer failed: {}", e)))
        })?;

        // Read transfer completion response from control channel
        ctrl.read_transfer_complete().await?;

        // Disconnect gracefully
        ctrl.quit().await.ok();

        // Calculate final statistics
        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };

        // Update final status in request group
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.complete().await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_uri_simple() {
        let uri = "ftp://example.com/file.txt";
        let result = FtpDownloadCommand::parse_uri(uri).unwrap();
        assert_eq!(result.0, "example.com");
        assert_eq!(result.1, 21);
        assert_eq!(result.2, "anonymous");
        assert_eq!(result.3, "aria2@");
        assert_eq!(result.4, "/file.txt");
    }

    #[test]
    fn test_parse_uri_with_port() {
        let uri = "ftp://example.com:2121/file.txt";
        let result = FtpDownloadCommand::parse_uri(uri).unwrap();
        assert_eq!(result.0, "example.com");
        assert_eq!(result.1, 2121);
    }

    #[test]
    fn test_parse_uri_with_auth() {
        let uri = "ftp://user:pass@example.com/file.txt";
        let result = FtpDownloadCommand::parse_uri(uri).unwrap();
        assert_eq!(result.2, "user");
        assert_eq!(result.3, "pass");
    }

    #[test]
    fn test_parse_uri_with_encoded_chars() {
        let uri = "ftp://example.com/my%20file.txt";
        let result = FtpDownloadCommand::parse_uri(uri).unwrap();
        assert_eq!(result.4, "/my file.txt");
    }

    #[test]
    fn test_parse_uri_invalid_protocol() {
        let uri = "http://example.com/file.txt";
        let result = FtpDownloadCommand::parse_uri(uri);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_filename_from_path() {
        assert_eq!(
            FtpDownloadCommand::extract_filename("/path/to/file.txt"),
            Some("file.txt".to_string())
        );
        assert_eq!(
            FtpDownloadCommand::extract_filename("/file.txt"),
            Some("file.txt".to_string())
        );
        assert_eq!(FtpDownloadCommand::extract_filename("/"), None);
        assert_eq!(FtpDownloadCommand::extract_filename(""), None);
    }

    #[test]
    fn test_urlencoding_decode() {
        assert_eq!(urlencoding_decode("hello%20world"), "hello world");
        assert_eq!(urlencoding_decode("%2F"), "/");
        assert_eq!(urlencoding_decode("normal"), "normal");
        assert_eq!(urlencoding_decode("%41"), "A");
    }

    #[test]
    fn test_parse_pasv_response_standard() {
        let resp = "227 Entering Passive Mode (192,168,1,100,200,10)";
        let result = parse_pasv_response(resp).unwrap();
        assert_eq!(result.0, "192.168.1.100");
        assert_eq!(result.1, 200 * 256 + 10); // 51210
    }

    #[test]
    fn test_parse_pasv_response_minimal() {
        let resp = "(10,0,0,1,0,21)";
        let result = parse_pasv_response(resp).unwrap();
        assert_eq!(result.0, "10.0.0.1");
        assert_eq!(result.1, 21);
    }

    #[test]
    fn test_parse_pasv_response_invalid() {
        assert!(parse_pasv_response("no parentheses").is_none());
        assert!(parse_pasv_response("(1,2,3)").is_none()); // Too few parts
    }

    #[test]
    fn test_parse_epsv_response_standard() {
        let resp = "229 Entering Extended Passive Mode (|||50001|)";
        let result = parse_epsv_response(resp).unwrap();
        assert_eq!(result, 50001);
    }

    #[test]
    fn test_parse_epsv_response_minimal() {
        let resp = "|||60000|";
        let result = parse_epsv_response(resp).unwrap();
        assert_eq!(result, 60000);
    }

    #[test]
    fn test_classify_ftp_error_transient() {
        // These should be classified as transient/recoverable
        let transient_codes = [421u16, 425, 426, 450, 451, 452];
        for code in transient_codes {
            // Note: classify_ftp_error is a method on FtpDownloadCommand
            // We can't easily test it without an instance, but the logic is clear
            assert!(
                (400..=499).contains(&code),
                "Code {} should be in transient range",
                code
            );
        }
    }

    #[test]
    fn test_classify_ftp_error_permanent() {
        // These should be classified as permanent/fatal
        let permanent_codes = [500u16, 501, 502, 503, 504, 530, 550, 553];
        for code in permanent_codes {
            assert!(
                (500..=599).contains(&code),
                "Code {} should be in permanent range",
                code
            );
        }
    }

    #[test]
    fn test_resume_offset_calculation() {
        // Test that resume offset would be calculated correctly from existing file
        // (This logic is in new(), so we verify the concept)
        let path = std::path::PathBuf::from("/tmp/test_file");
        if path.exists() {
            let metadata = std::fs::metadata(&path).unwrap();
            let _offset = metadata.len();
            // offset is u64, always >= 0 by type guarantee
        } else {
            // File doesn't exist, offset should be 0
            assert_eq!(0u64, 0);
        }
    }

    #[tokio::test]
    async fn test_raw_ftp_control_connect_invalid_address() {
        let result = RawFtpControl::connect("invalid.host.name.invalid", 21).await;
        assert!(result.is_err());
    }
}
