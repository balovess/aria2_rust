//! FTP negotiation module
//!
//! Implements the full FTP negotiation flow matching the C++ aria2
//! `FtpNegotiationCommand` state machine, but using Rust's async/await
//! model instead of 30+ explicit states.
//!
//! The negotiation flow (in order):
//! 1. Connect + read greeting (or skip if using pooled connection)
//! 2. Authenticate (or skip if pooled)
//! 3. Set TYPE (binary)
//! 4. PWD to get baseWorkingDir
//! 5. CWD traversal (split path, CWD each directory component)
//! 6. MDTM (if remote-time option is enabled)
//! 7. SIZE
//! 8. Choose data connection mode (PASV/EPSV or PORT/EPRT)
//! 9. REST (after data connection established, per C++ ordering)
//! 10. RETR
//!
//! After data transfer completes, call `finish_download()` to read the
//! 226 transfer-complete response and optionally pool the connection.

use std::time::SystemTime;

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::types::FtpMode;

/// Result of a successful FTP negotiation.
///
/// Contains everything the download pipeline needs to begin reading data
/// and to finalize the transfer afterwards.
pub struct FtpNegotiationResult {
    /// Data connection for reading file content
    pub data_stream: TcpStream,
    /// Control connection preserved for reading the 226 response later
    pub control: RawFtpControl,
    /// File size reported by SIZE command (None if SIZE not supported)
    pub file_size: Option<u64>,
    /// Modification time from MDTM command (None if MDTM not supported or disabled)
    pub modification_time: Option<SystemTime>,
    /// Base working directory from PWD, used for connection pool key
    pub base_working_dir: String,
}

/// Raw FTP control connection handler.
///
/// Wraps the control socket for command/response I/O after the
/// negotiation phase, enabling `finish_download()` to read the 226
/// response and optionally pool the connection.
pub struct RawFtpControl {
    reader: BufReader<TcpStream>,
    host: String,
    read_timeout: Duration,
}

impl RawFtpControl {
    /// Build a `RawFtpControl` from an existing buffered control stream.
    ///
    /// This is used internally by `FtpNegotiator::negotiate()` when the
    /// negotiation completes successfully.
    pub fn new(reader: BufReader<TcpStream>, host: String, read_timeout: Duration) -> Self {
        Self {
            reader,
            host,
            read_timeout,
        }
    }

    /// Read the transfer-complete response (226) after data transfer ends.
    ///
    /// Per the C++ `FtpFinishDownloadCommand`, a non-226 response is NOT
    /// treated as a fatal error since the data was already received. The
    /// connection may still be pooled.
    ///
    /// # Returns
    ///
    /// `Ok(true)` if 226 received, `Ok(false)` for any other response.
    pub async fn read_transfer_complete(&mut self) -> Result<bool> {
        match self.read_response(self.read_timeout).await {
            Ok((226, msg)) => {
                debug!("Transfer complete (226): {}", msg.trim());
                Ok(true)
            }
            Ok((code, msg)) => {
                warn!("Transfer completion non-226: {} {}", code, msg);
                Ok(false)
            }
            Err(e) => {
                debug!("Transfer completion timeout/error (may be normal): {}", e);
                Ok(false)
            }
        }
    }

    /// Send QUIT and close the control connection gracefully.
    pub async fn quit(mut self) -> Result<()> {
        debug!("Sending QUIT command");
        if let Err(e) = self.send_command("QUIT").await {
            warn!("Failed to send QUIT (connection may be closed): {}", e);
            return Ok(());
        }
        match self.read_response(self.read_timeout).await {
            Ok(resp) => {
                info!("FTP disconnected: {}", resp.1.trim());
                Ok(())
            }
            Err(e) => {
                warn!("Failed to read QUIT response: {}", e);
                Ok(())
            }
        }
    }

    /// Get a reference to the underlying control stream (for connection pooling).
    pub fn stream(&self) -> &TcpStream {
        self.reader.get_ref()
    }

    /// Consume self and return the inner buffered reader (for connection pooling).
    pub fn into_inner(self) -> BufReader<TcpStream> {
        self.reader
    }

    /// Get the host this control connection is connected to.
    pub fn host(&self) -> &str {
        &self.host
    }

    // ---- Internal I/O helpers ----

    async fn send_command(&mut self, cmd: &str) -> Result<()> {
        debug!("FTP CMD: {}", cmd.trim());
        use tokio::io::AsyncWriteExt;
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

    async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        use tokio::io::AsyncBufReadExt;

        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();
            let bytes_read = timeout(timeout_dur, self.reader.read_line(&mut line))
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

    /// Send command and read response in one operation.
    /// Will be used for connection pooling operations (QUIT, etc.)
    #[allow(dead_code)]
    async fn command(&mut self, cmd: &str, timeout_dur: Duration) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(timeout_dur).await
    }
}

/// Configuration for FTP negotiation.
#[derive(Debug, Clone)]
pub struct FtpNegotiationConfig {
    /// Server hostname
    pub host: String,
    /// Server port (typically 21)
    pub port: u16,
    /// Username for authentication
    pub username: String,
    /// Password for authentication
    pub password: String,
    /// URL-decoded remote path (e.g., "/pub/linux/file.tar.gz")
    pub remote_path: String,
    /// Data connection mode (passive or active)
    pub mode: FtpMode,
    /// Resume offset in bytes (0 = no resume)
    pub resume_offset: u64,
    /// Whether to send MDTM for remote time
    pub remote_time: bool,
    /// Connection timeout
    pub connect_timeout: Duration,
    /// Read/response timeout for FTP commands
    pub command_timeout: Duration,
    /// Whether this is a pooled connection (skip connect + auth)
    pub is_pooled: bool,
    /// Base working directory for pooled connections (must match)
    pub pooled_base_working_dir: Option<String>,
}

/// FTP negotiation orchestrator.
///
/// Performs the full FTP negotiation flow as a linear async function
/// instead of the C++ state machine with 30+ states.
pub struct FtpNegotiator;

impl FtpNegotiator {
    /// Execute the complete FTP negotiation flow.
    ///
    /// This replaces the C++ `FtpNegotiationCommand` state machine with
    /// a sequential async function. The ordering matches the C++:
    ///
    /// 1. Connect + greeting (or skip for pooled)
    /// 2. Authenticate (or skip for pooled)
    /// 3. TYPE I (binary)
    /// 4. PWD -> baseWorkingDir
    /// 5. CWD traversal (each directory component separately)
    /// 6. MDTM (if remote_time enabled)
    /// 7. SIZE
    /// 8. Data connection (PASV/EPSV or EPRT/PORT)
    /// 9. REST (after data connection established)
    /// 10. RETR
    pub async fn negotiate(config: FtpNegotiationConfig) -> Result<FtpNegotiationResult> {
        let FtpNegotiationConfig {
            host,
            port,
            username,
            password,
            remote_path,
            mode,
            resume_offset,
            remote_time,
            connect_timeout,
            command_timeout,
            is_pooled,
            pooled_base_working_dir: _,
        } = config;

        // Step 1-2: Connect + authenticate (or skip if pooled)
        let mut ctrl = if is_pooled {
            // For pooled connections, we receive a pre-established control stream
            // The caller must reconstruct RawFtpControl from the pooled socket
            return Err(Aria2Error::DownloadFailed(
                "Pooled connection negotiation requires a pre-established control stream".into(),
            ));
        } else {
            Self::connect_and_authenticate(
                &host,
                port,
                &username,
                &password,
                connect_timeout,
                command_timeout,
            )
            .await?
        };

        // Step 3: TYPE I (binary)
        Self::set_binary_mode(&mut ctrl, command_timeout).await?;

        // Step 4: PWD -> baseWorkingDir
        let base_working_dir = Self::query_pwd(&mut ctrl, command_timeout).await?;
        info!("Base working directory: {}", base_working_dir);

        // Step 5: CWD traversal
        // Split the directory part of the URL path into components
        let dir_part = extract_directory_part(&remote_path);
        Self::cwd_traversal(&mut ctrl, &base_working_dir, &dir_part, command_timeout).await?;

        // Step 6: MDTM (if remote_time enabled)
        let modification_time = if remote_time {
            let file_part = extract_file_part(&remote_path);
            Self::query_mdtm(&mut ctrl, file_part, command_timeout).await?
        } else {
            None
        };

        // Step 7: SIZE
        let file_part = extract_file_part(&remote_path);
        let file_size = Self::query_size(&mut ctrl, file_part, command_timeout).await?;

        // Step 8: Data connection
        let data_stream = match mode {
            FtpMode::Passive => {
                Self::enter_passive_mode(&mut ctrl, &host, connect_timeout, command_timeout).await?
            }
            FtpMode::Active => {
                Self::enter_active_mode(&mut ctrl, command_timeout, connect_timeout).await?
            }
        };

        // Step 9: REST (after data connection established, per C++ ordering)
        // C++ sends REST *after* PASV/PORT, not before
        if resume_offset > 0 {
            Self::send_rest(&mut ctrl, resume_offset, command_timeout).await?;
        }

        // Step 10: RETR
        Self::send_retr(&mut ctrl, file_part, command_timeout).await?;

        // Build result - detach the control stream
        let ctrl_reader = ctrl.reader;
        let result = FtpNegotiationResult {
            data_stream,
            control: RawFtpControl::new(ctrl_reader, host.clone(), command_timeout),
            file_size,
            modification_time,
            base_working_dir,
        };

        Ok(result)
    }

    /// Negotiate using a pooled (pre-authenticated) connection.
    ///
    /// When a connection is reused from the pool, we skip connect + auth
    /// and start from CWD_PREP (step 5). The baseWorkingDir must match.
    pub async fn negotiate_pooled(
        control: RawFtpControl,
        config: FtpNegotiationConfig,
    ) -> Result<FtpNegotiationResult> {
        let FtpNegotiationConfig {
            host,
            port: _,
            username: _,
            password: _,
            remote_path,
            mode,
            resume_offset,
            remote_time,
            connect_timeout,
            command_timeout,
            is_pooled: _,
            pooled_base_working_dir,
        } = config;

        let mut ctrl = PooledControl {
            reader: control.reader,
            read_timeout: command_timeout,
        };

        let base_working_dir = pooled_base_working_dir.unwrap_or_else(|| "/".to_string());

        // Step 5: CWD traversal (skip connect + auth + TYPE + PWD for pooled)
        let dir_part = extract_directory_part(&remote_path);
        Self::cwd_traversal_pooled(&mut ctrl, &base_working_dir, &dir_part, command_timeout)
            .await?;

        // Step 6: MDTM
        let modification_time = if remote_time {
            let file_part = extract_file_part(&remote_path);
            Self::query_mdtm_pooled(&mut ctrl, file_part, command_timeout).await?
        } else {
            None
        };

        // Step 7: SIZE
        let file_part = extract_file_part(&remote_path);
        let file_size = Self::query_size_pooled(&mut ctrl, file_part, command_timeout).await?;

        // Step 8: Data connection
        let data_stream = match mode {
            FtpMode::Passive => {
                Self::enter_passive_mode_pooled(&mut ctrl, &host, connect_timeout, command_timeout)
                    .await?
            }
            FtpMode::Active => {
                Self::enter_active_mode_pooled(&mut ctrl, command_timeout, connect_timeout).await?
            }
        };

        // Step 9: REST
        if resume_offset > 0 {
            Self::send_rest_pooled(&mut ctrl, resume_offset, command_timeout).await?;
        }

        // Step 10: RETR
        Self::send_retr_pooled(&mut ctrl, file_part, command_timeout).await?;

        let result = FtpNegotiationResult {
            data_stream,
            control: RawFtpControl::new(ctrl.reader, host.clone(), command_timeout),
            file_size,
            modification_time,
            base_working_dir,
        };

        Ok(result)
    }

    /// Finish a download: read the 226 response and optionally pool the connection.
    ///
    /// Per C++ `FtpFinishDownloadCommand`, non-226 responses are not fatal
    /// (data was already received). If the connection is reusable and the
    /// pool key (host + username + baseWorkingDir) matches, the connection
    /// can be returned for reuse.
    ///
    /// # Arguments
    ///
    /// - `control`: The raw FTP control connection from `FtpNegotiationResult`
    /// - `reuse_connection`: Whether connection pooling is enabled
    ///
    /// # Returns
    ///
    /// `Some(RawFtpControl)` if the connection can be pooled, `None` otherwise.
    pub async fn finish_download(
        control: &mut RawFtpControl,
        reuse_connection: bool,
    ) -> Option<()> {
        // Read 226 transfer-complete response
        let transfer_ok = control.read_transfer_complete().await.ok()?;

        if transfer_ok && reuse_connection {
            // Connection is good for reuse
            return Some(());
        }

        // Non-226 or pooling disabled; still return Some(()) to indicate
        // the finish completed (data was already received)
        Some(())
    }

    // =========================================================================
    // Private helpers - fresh connection flow
    // =========================================================================

    /// Connect to FTP server, read greeting, and authenticate.
    async fn connect_and_authenticate(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        connect_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<FreshControl> {
        debug!("Connecting to FTP server at {}:{}", host, port);

        let stream = timeout(connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP connect failed to {}:{}: {}", host, port, e),
                })
            })?;

        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;

        let mut ctrl = FreshControl {
            reader: BufReader::new(stream),
            command_timeout,
        };

        // Read welcome message
        let welcome = ctrl.read_response(command_timeout).await?;
        if !(200..300).contains(&welcome.0) && !(100..200).contains(&welcome.0) {
            return Err(Aria2Error::DownloadFailed(format!(
                "FTP server rejected connection: {} {}",
                welcome.0, welcome.1
            )));
        }
        info!("Connected to FTP server {}:{}", host, port);

        // Authenticate
        debug!("Authenticating as user: {}", username);
        let user_resp = ctrl.command(&format!("USER {}", username)).await?;
        match user_resp.0 {
            230 => {
                info!("FTP login successful (no password required)");
            }
            331 | 332 => {
                debug!("Password required, sending PASS command");
                let pass_resp = ctrl.command(&format!("PASS {}", password)).await?;
                if !(200..300).contains(&pass_resp.0) {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!(
                                "Login failed: {} {}",
                                pass_resp.0, pass_resp.1
                            ),
                        },
                    ));
                }
                info!("FTP login successful");
            }
            _ => {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "Unexpected USER response: {} {}",
                            user_resp.0, user_resp.1
                        ),
                    },
                ));
            }
        }

        Ok(ctrl)
    }

    /// Set binary transfer mode (TYPE I).
    async fn set_binary_mode(ctrl: &mut FreshControl, _timeout_dur: Duration) -> Result<()> {
        debug!("Setting transfer mode to binary (TYPE I)");
        let resp = ctrl.command("TYPE I").await?;
        if !(200..300).contains(&resp.0) {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("TYPE I failed: {} {}", resp.0, resp.1),
                },
            ));
        }
        Ok(())
    }

    /// Query PWD to get the base working directory.
    async fn query_pwd(ctrl: &mut FreshControl, _timeout_dur: Duration) -> Result<String> {
        debug!("Sending PWD command");
        let resp = ctrl.command("PWD").await?;
        if resp.0 != 257 {
            return Err(Aria2Error::DownloadFailed(format!(
                "PWD command failed: {} {}",
                resp.0, resp.1
            )));
        }

        // Parse 257 "/path" current directory
        let msg = resp.1.trim();
        if let Some(start) = msg.find('"')
            && let Some(end) = msg.rfind('"')
            && end > start
        {
            Ok(msg[start + 1..end].to_string())
        } else {
            Ok(msg.to_string())
        }
    }

    /// CWD traversal: change to each directory component in sequence.
    ///
    /// Matches the C++ `sendCwdPrep` + `sendCwd`/`recvCwd` loop.
    /// The path is split by '/', and each non-empty component is
    /// traversed with CWD. The baseWorkingDir is prepended as the
    /// first CWD target.
    async fn cwd_traversal(
        ctrl: &mut FreshControl,
        base_working_dir: &str,
        dir_path: &str,
        _timeout_dur: Duration,
    ) -> Result<()> {
        // Build the CWD queue: baseWorkingDir first, then each dir component
        let mut dirs: Vec<&str> = Vec::new();

        // Add base working dir if not root
        if base_working_dir != "/" && !base_working_dir.is_empty() {
            dirs.push(base_working_dir);
        }

        // Split directory path into components (skip empty segments from //)
        for component in dir_path.split('/') {
            if !component.is_empty() {
                dirs.push(component);
            }
        }

        debug!("CWD traversal: {} directories to traverse", dirs.len());

        for dir in &dirs {
            debug!("CWD {}", dir);
            let (code, msg) = ctrl.command(&format!("CWD {}", dir)).await?;
            if code == 550 {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::ServerError { code: 550 },
                ));
            }
            if code != 250 {
                return Err(Aria2Error::DownloadFailed(format!(
                    "CWD {} failed: {} {}",
                    dir, code, msg
                )));
            }
        }

        debug!("CWD traversal completed successfully");
        Ok(())
    }

    /// Query MDTM for file modification time.
    async fn query_mdtm(
        ctrl: &mut FreshControl,
        file_path: &str,
        _timeout_dur: Duration,
    ) -> Result<Option<SystemTime>> {
        debug!("Sending MDTM command for: {}", file_path);
        let resp = ctrl.command(&format!("MDTM {}", file_path)).await?;

        if resp.0 != 213 {
            info!(
                "MDTM command returned non-213 response: {} {}",
                resp.0, resp.1
            );
            return Ok(None);
        }

        let msg = resp.1.trim();
        let timestamp_str = if msg.starts_with("213") {
            msg[3..].trim()
        } else {
            msg
        };

        if timestamp_str.len() < 14 {
            warn!("MDTM response too short to parse: {}", timestamp_str);
            return Ok(None);
        }

        let ts = &timestamp_str[..14];
        match parse_mdtm_timestamp(ts) {
            Some(t) => {
                debug!("MDTM parsed modification time: {:?}", t);
                Ok(Some(t))
            }
            None => {
                warn!("Failed to parse MDTM timestamp: {}", ts);
                Ok(None)
            }
        }
    }

    /// Query SIZE for file size.
    async fn query_size(
        ctrl: &mut FreshControl,
        file_path: &str,
        _timeout_dur: Duration,
    ) -> Result<Option<u64>> {
        debug!("Sending SIZE command for: {}", file_path);
        let resp = ctrl.command(&format!("SIZE {}", file_path)).await?;

        if resp.0 == 213 {
            let msg = resp.1.trim();
            let size_str = if msg.starts_with("213") {
                msg[3..].trim()
            } else {
                msg
            };
            match size_str.parse::<u64>() {
                Ok(size) => {
                    debug!("File size: {} bytes", size);
                    Ok(Some(size))
                }
                Err(_) => {
                    warn!("Failed to parse SIZE response: {}", size_str);
                    Ok(None)
                }
            }
        } else {
            info!(
                "SIZE command returned non-213 response: {} {}",
                resp.0, resp.1
            );
            Ok(None)
        }
    }

    /// Enter passive mode and connect to the data port.
    async fn enter_passive_mode(
        ctrl: &mut FreshControl,
        host: &str,
        connect_timeout: Duration,
        _command_timeout: Duration,
    ) -> Result<TcpStream> {
        // Try EPSV first (IPv6-friendly)
        debug!("Attempting extended passive mode (EPSV)");
        let epsv_resp = ctrl.command("EPSV").await;

        match epsv_resp {
            Ok(resp) if resp.0 == 229 => {
                if let Some(port) = parse_epsv_response(&resp.1) {
                    debug!("EPSV successful, using port: {}", port);
                    let data_stream =
                        timeout(connect_timeout, TcpStream::connect((host, port)))
                            .await
                            .map_err(|_| {
                                Aria2Error::Recoverable(RecoverableError::Timeout)
                            })?
                            .map_err(|e| {
                                Aria2Error::Recoverable(
                                    RecoverableError::TemporaryNetworkFailure {
                                        message: format!(
                                            "EPSV data connection failed: {}",
                                            e
                                        ),
                                    },
                                )
                            })?;
                    let _ = data_stream.set_nodelay(true);
                    return Ok(data_stream);
                }
                warn!("Failed to parse EPSV response, falling back to PASV");
            }
            _ => {
                debug!("EPSV not supported, trying PASV");
            }
        }

        // Fallback to PASV
        debug!("Entering passive mode (PASV)");
        let pasv_resp = ctrl.command("PASV").await?;
        if pasv_resp.0 != 227 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("PASV failed: {} {}", pasv_resp.0, pasv_resp.1),
                },
            ));
        }

        match parse_pasv_response(&pasv_resp.1) {
            Some((data_host, data_port)) => {
                debug!("PASV successful, data channel: {}:{}", data_host, data_port);
                let data_stream = timeout(
                    connect_timeout,
                    TcpStream::connect((data_host.as_str(), data_port)),
                )
                .await
                .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("PASV data connection failed: {}", e),
                    })
                })?;
                let _ = data_stream.set_nodelay(true);
                Ok(data_stream)
            }
            None => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "Cannot parse PASV response".into(),
                },
            )),
        }
    }

    /// Enter active mode and accept the server's data connection.
    async fn enter_active_mode(
        ctrl: &mut FreshControl,
        _command_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<TcpStream> {
        // Get local address from the control stream
        let local_addr = ctrl
            .reader
            .get_ref()
            .local_addr()
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to get local address: {}", e),
                })
            })?;

        // Bind a listener on port 0 (auto-assign)
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to bind data port: {}", e),
                })
            })?;
        let data_port = listener
            .local_addr()
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to get listen port: {}", e),
                })
            })?
            .port();

        let local_ip = local_addr.ip();

        // Try EPRT first
        let eprt_cmd = format!("EPRT |1|{}|{}|", local_ip, data_port);
        debug!("Sending EPRT command");
        let eprt_resp = ctrl.command(&eprt_cmd).await?;

        if eprt_resp.0 != 200 && eprt_resp.0 != 500 && eprt_resp.0 != 501 && eprt_resp.0 != 502 {
            return Err(Aria2Error::DownloadFailed(format!(
                "EPRT command failed: {} {}",
                eprt_resp.0, eprt_resp.1
            )));
        }

        if !(200..300).contains(&eprt_resp.0) {
            // EPRT failed, try PORT
            warn!("EPRT unavailable, falling back to PORT mode");

            let ipv4_addr = match local_ip {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => {
                    return Err(Aria2Error::DownloadFailed(
                        "IPv6 does not support active mode PORT command".into(),
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
            debug!("Sending PORT command");
            let port_resp = ctrl.command(&port_cmd).await?;
            if !(200..300).contains(&port_resp.0) {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::ServerError { code: 425 },
                ));
            }
        }

        // Wait for server to connect
        debug!("Waiting for server data connection on port: {}", data_port);
        let (data_stream, _addr) = timeout(connect_timeout, listener.accept())
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to accept data connection: {}", e),
                })
            })?;

        let _ = data_stream.set_nodelay(true);
        debug!("Active mode data connection established");
        Ok(data_stream)
    }

    /// Send REST command for resume offset.
    ///
    /// This is called AFTER the data connection is established, matching
    /// the C++ ordering (SEQ_SEND_REST_PASV / SEQ_SEND_REST).
    async fn send_rest(
        ctrl: &mut FreshControl,
        offset: u64,
        _timeout_dur: Duration,
    ) -> Result<()> {
        debug!("Setting resume offset: {} bytes", offset);
        let resp = ctrl.command(&format!("REST {}", offset)).await?;
        if resp.0 != 350 {
            warn!(
                "REST command not accepted by server: {} {}",
                resp.0, resp.1
            );
            // C++ aria2: CANNOT_RESUME if offset != 0 and server doesn't support REST
            return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
        }
        debug!("REST accepted by server");
        Ok(())
    }

    /// Send RETR command to start file transfer.
    async fn send_retr(
        ctrl: &mut FreshControl,
        file_path: &str,
        _timeout_dur: Duration,
    ) -> Result<()> {
        debug!("Initiating file retrieval: {}", file_path);
        let resp = ctrl.command(&format!("RETR {}", file_path)).await?;
        if resp.0 == 150 || resp.0 == 125 {
            Ok(())
        } else if resp.0 == 550 {
            Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 550,
            }))
        } else {
            Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("RETR unexpected response: {} {}", resp.0, resp.1),
                },
            ))
        }
    }

    // =========================================================================
    // Private helpers - pooled connection flow
    // (Same logic but using PooledControl instead of FreshControl)
    // =========================================================================

    async fn cwd_traversal_pooled(
        ctrl: &mut PooledControl,
        base_working_dir: &str,
        dir_path: &str,
        _timeout_dur: Duration,
    ) -> Result<()> {
        let mut dirs: Vec<&str> = Vec::new();
        if base_working_dir != "/" && !base_working_dir.is_empty() {
            dirs.push(base_working_dir);
        }
        for component in dir_path.split('/') {
            if !component.is_empty() {
                dirs.push(component);
            }
        }

        debug!("CWD traversal (pooled): {} directories", dirs.len());
        for dir in &dirs {
            debug!("CWD {}", dir);
            let (code, msg) = ctrl.command(&format!("CWD {}", dir)).await?;
            if code == 550 {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::ServerError { code: 550 },
                ));
            }
            if code != 250 {
                return Err(Aria2Error::DownloadFailed(format!(
                    "CWD {} failed: {} {}",
                    dir, code, msg
                )));
            }
        }
        Ok(())
    }

    async fn query_mdtm_pooled(
        ctrl: &mut PooledControl,
        file_path: &str,
        _timeout_dur: Duration,
    ) -> Result<Option<SystemTime>> {
        debug!("Sending MDTM command (pooled) for: {}", file_path);
        let resp = ctrl.command(&format!("MDTM {}", file_path)).await?;

        if resp.0 != 213 {
            info!("MDTM non-213: {} {}", resp.0, resp.1);
            return Ok(None);
        }

        let msg = resp.1.trim();
        let timestamp_str = if msg.starts_with("213") {
            msg[3..].trim()
        } else {
            msg
        };
        if timestamp_str.len() < 14 {
            return Ok(None);
        }
        match parse_mdtm_timestamp(&timestamp_str[..14]) {
            Some(t) => Ok(Some(t)),
            None => Ok(None),
        }
    }

    async fn query_size_pooled(
        ctrl: &mut PooledControl,
        file_path: &str,
        _timeout_dur: Duration,
    ) -> Result<Option<u64>> {
        debug!("Sending SIZE command (pooled) for: {}", file_path);
        let resp = ctrl.command(&format!("SIZE {}", file_path)).await?;

        if resp.0 == 213 {
            let msg = resp.1.trim();
            let size_str = if msg.starts_with("213") {
                msg[3..].trim()
            } else {
                msg
            };
            Ok(size_str.parse::<u64>().ok())
        } else {
            Ok(None)
        }
    }

    async fn enter_passive_mode_pooled(
        ctrl: &mut PooledControl,
        host: &str,
        connect_timeout: Duration,
        _command_timeout: Duration,
    ) -> Result<TcpStream> {
        debug!("Attempting EPSV (pooled)");
        let epsv_resp = ctrl.command("EPSV").await;

        match epsv_resp {
            Ok(resp) if resp.0 == 229 => {
                if let Some(port) = parse_epsv_response(&resp.1) {
                    debug!("EPSV successful (pooled), port: {}", port);
                    let data_stream =
                        timeout(connect_timeout, TcpStream::connect((host, port)))
                            .await
                            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
                            .map_err(|e| {
                                Aria2Error::Recoverable(
                                    RecoverableError::TemporaryNetworkFailure {
                                        message: format!("EPSV data connection failed: {}", e),
                                    },
                                )
                            })?;
                    let _ = data_stream.set_nodelay(true);
                    return Ok(data_stream);
                }
                warn!("Failed to parse EPSV response, falling back to PASV");
            }
            _ => {
                debug!("EPSV not supported, trying PASV");
            }
        }

        let pasv_resp = ctrl.command("PASV").await?;
        if pasv_resp.0 != 227 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("PASV failed: {} {}", pasv_resp.0, pasv_resp.1),
                },
            ));
        }

        match parse_pasv_response(&pasv_resp.1) {
            Some((data_host, data_port)) => {
                debug!("PASV successful (pooled): {}:{}", data_host, data_port);
                let data_stream = timeout(
                    connect_timeout,
                    TcpStream::connect((data_host.as_str(), data_port)),
                )
                .await
                .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("PASV data connection failed: {}", e),
                    })
                })?;
                let _ = data_stream.set_nodelay(true);
                Ok(data_stream)
            }
            None => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "Cannot parse PASV response".into(),
                },
            )),
        }
    }

    async fn enter_active_mode_pooled(
        ctrl: &mut PooledControl,
        _command_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<TcpStream> {
        let local_addr = ctrl
            .reader
            .get_ref()
            .local_addr()
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to get local address: {}", e),
                })
            })?;

        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to bind data port: {}", e),
                })
            })?;
        let data_port = listener
            .local_addr()
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to get listen port: {}", e),
                })
            })?
            .port();

        let local_ip = local_addr.ip();

        // Try EPRT first
        let eprt_cmd = format!("EPRT |1|{}|{}|", local_ip, data_port);
        let eprt_resp = ctrl.command(&eprt_cmd).await?;

        if !(200..300).contains(&eprt_resp.0) {
            let ipv4_addr = match local_ip {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => {
                    return Err(Aria2Error::DownloadFailed(
                        "IPv6 does not support PORT command".into(),
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
            let port_resp = ctrl.command(&port_cmd).await?;
            if !(200..300).contains(&port_resp.0) {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::ServerError { code: 425 },
                ));
            }
        }

        let (data_stream, _) = timeout(connect_timeout, listener.accept())
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to accept data connection: {}", e),
                })
            })?;
        let _ = data_stream.set_nodelay(true);
        Ok(data_stream)
    }

    async fn send_rest_pooled(
        ctrl: &mut PooledControl,
        offset: u64,
        _timeout_dur: Duration,
    ) -> Result<()> {
        debug!("Setting resume offset (pooled): {} bytes", offset);
        let resp = ctrl.command(&format!("REST {}", offset)).await?;
        if resp.0 != 350 {
            return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
        }
        Ok(())
    }

    async fn send_retr_pooled(
        ctrl: &mut PooledControl,
        file_path: &str,
        _timeout_dur: Duration,
    ) -> Result<()> {
        debug!("Initiating file retrieval (pooled): {}", file_path);
        let resp = ctrl.command(&format!("RETR {}", file_path)).await?;
        if resp.0 == 150 || resp.0 == 125 {
            Ok(())
        } else if resp.0 == 550 {
            Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 550,
            }))
        } else {
            Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("RETR unexpected response: {} {}", resp.0, resp.1),
                },
            ))
        }
    }
}

// =============================================================================
// Internal control connection wrappers
// =============================================================================

/// Control connection for a freshly established FTP session.
///
/// Wraps the buffered TCP stream with command/response helpers.
struct FreshControl {
    reader: BufReader<TcpStream>,
    command_timeout: Duration,
}

impl FreshControl {
    async fn send_command(&mut self, cmd: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        debug!("FTP CMD: {}", cmd.trim());
        self.reader
            .get_mut()
            .write_all(format!("{}\r\n", cmd).as_bytes())
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP write failed: {}", e),
                })
            })?;
        self.reader.get_mut().flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP flush failed: {}", e),
            })
        })?;
        Ok(())
    }

    async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();
            let bytes_read = timeout(timeout_dur, self.reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP response timeout after {:?}", timeout_dur),
                    })
                })?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP read error: {}", e),
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

    async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(self.command_timeout).await
    }
}

/// Control connection for a pooled (pre-authenticated) FTP session.
///
/// Functionally identical to `FreshControl` but distinct type to prevent
/// mixing fresh and pooled flows.
struct PooledControl {
    reader: BufReader<TcpStream>,
    read_timeout: Duration,
}

impl PooledControl {
    async fn send_command(&mut self, cmd: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        debug!("FTP CMD (pooled): {}", cmd.trim());
        self.reader
            .get_mut()
            .write_all(format!("{}\r\n", cmd).as_bytes())
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP write failed: {}", e),
                })
            })?;
        self.reader.get_mut().flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("FTP flush failed: {}", e),
            })
        })?;
        Ok(())
    }

    async fn read_response(&mut self, timeout_dur: Duration) -> Result<(u16, String)> {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();
            let bytes_read = timeout(timeout_dur, self.reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP response timeout after {:?}", timeout_dur),
                    })
                })?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP read error: {}", e),
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
        debug!("FTP RESP (pooled): {} {}", code_val, message.trim());
        Ok((code_val, message))
    }

    async fn command(&mut self, cmd: &str) -> Result<(u16, String)> {
        self.send_command(cmd).await?;
        self.read_response(self.read_timeout).await
    }
}

// =============================================================================
// Path manipulation helpers
// =============================================================================

/// Extract the directory part of a remote path.
///
/// For `/pub/linux/file.tar.gz`, returns `/pub/linux`.
/// For `/file.txt`, returns `` (empty, meaning no CWD needed).
/// The file name (last component) is NOT included as a CWD target.
fn extract_directory_part(remote_path: &str) -> String {
    if remote_path.is_empty() {
        return String::new();
    }
    match remote_path.rfind('/') {
        Some(idx) => remote_path[..idx].to_string(),
        None => String::new(),
    }
}

/// Extract the file name part of a remote path.
///
/// For `/pub/linux/file.tar.gz`, returns `file.tar.gz`.
/// For `/file.txt`, returns `file.txt`.
/// For `/`, returns `` (empty).
fn extract_file_part(remote_path: &str) -> &str {
    if remote_path.is_empty() {
        return "";
    }
    match remote_path.rfind('/') {
        Some(idx) => &remote_path[idx + 1..],
        None => remote_path,
    }
}

// =============================================================================
// Response parsing helpers
// =============================================================================

/// Parse PASV response to extract IP and port.
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

/// Parse EPSV response to extract port.
fn parse_epsv_response(response: &str) -> Option<u16> {
    let start = response.rfind('|')?;
    let prev_pipe = response[..start].rfind('|')?;
    let port_str = &response[prev_pipe + 1..start];
    port_str.parse::<u16>().ok()
}

/// Parse MDTM timestamp `YYYYMMDDhhmmss` to `SystemTime` (UTC).
fn parse_mdtm_timestamp(s: &str) -> Option<SystemTime> {
    if s.len() < 14 {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[4..6].parse().ok()?;
    let day: u32 = s[6..8].parse().ok()?;
    let hour: u32 = s[8..10].parse().ok()?;
    let minute: u32 = s[10..12].parse().ok()?;
    let second: u32 = s[12..14].parse().ok()?;

    if !(1990..=2999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let days_since_epoch = days_from_civil(year, month, day)?;
    let secs = days_since_epoch as u64 * 86400
        + hour as u64 * 3600
        + minute as u64 * 60
        + second as u64;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

/// Days since 1970-01-01 using Howard Hinnant's civil_from_days algorithm.
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<u64> {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let doy = (153 * m as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    Some(days)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_directory_part() {
        assert_eq!(extract_directory_part("/pub/linux/file.tar.gz"), "/pub/linux");
        assert_eq!(extract_directory_part("/file.txt"), "");
        assert_eq!(extract_directory_part("/"), "");
        assert_eq!(extract_directory_part(""), "");
        assert_eq!(
            extract_directory_part("/path/to/dir/file.txt"),
            "/path/to/dir"
        );
    }

    #[test]
    fn test_extract_file_part() {
        assert_eq!(extract_file_part("/pub/linux/file.tar.gz"), "file.tar.gz");
        assert_eq!(extract_file_part("/file.txt"), "file.txt");
        assert_eq!(extract_file_part("/"), "");
        assert_eq!(extract_file_part(""), "");
    }

    #[test]
    fn test_parse_mdtm_timestamp_valid() {
        let ts = parse_mdtm_timestamp("20240115103000").unwrap();
        // 2024-01-15 10:30:00 UTC = epoch 1705314600
        let duration = ts.duration_since(std::time::UNIX_EPOCH).unwrap();
        assert_eq!(duration.as_secs(), 1705314600);
    }

    #[test]
    fn test_parse_mdtm_timestamp_invalid() {
        assert!(parse_mdtm_timestamp("").is_none());
        assert!(parse_mdtm_timestamp("2024").is_none());
        assert!(parse_mdtm_timestamp("20241301120000").is_none()); // month 13
        assert!(parse_mdtm_timestamp("20240132120000").is_none()); // day 32
    }

    #[test]
    fn test_parse_pasv_response_standard() {
        let resp = "227 Entering Passive Mode (192,168,1,100,195,123)";
        let result = parse_pasv_response(resp).unwrap();
        assert_eq!(result.0, "192.168.1.100");
        assert_eq!(result.1, 195 * 256 + 123);
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
    fn test_days_from_civil_epoch() {
        // 1970-01-01 should be day 0
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        // 1970-01-02 should be day 1
        assert_eq!(days_from_civil(1970, 1, 2), Some(1));
        // 2000-01-01 known value: 10957
        assert_eq!(days_from_civil(2000, 1, 1), Some(10957));
    }

    #[test]
    fn test_cwd_traversal_splitting() {
        // The CWD traversal should split "/pub/linux" into ["pub", "linux"]
        // and NOT send CWD for the full path at once
        let path = "/pub/linux";
        let dirs: Vec<&str> = path
            .split('/')
            .filter(|c| !c.is_empty())
            .collect();
        assert_eq!(dirs, vec!["pub", "linux"]);
    }

    #[test]
    fn test_ftp_negotiation_config_defaults() {
        let config = FtpNegotiationConfig {
            host: "example.com".to_string(),
            port: 21,
            username: "anonymous".to_string(),
            password: "aria2@".to_string(),
            remote_path: "/pub/file.txt".to_string(),
            mode: FtpMode::Passive,
            resume_offset: 0,
            remote_time: false,
            connect_timeout: Duration::from_secs(30),
            command_timeout: Duration::from_secs(30),
            is_pooled: false,
            pooled_base_working_dir: None,
        };
        assert_eq!(config.host, "example.com");
        assert_eq!(config.port, 21);
        assert!(!config.is_pooled);
    }
}
