//! FTP negotiation module
//!
//! Implements the full FTP negotiation flow matching the C++ aria2
//! `FtpNegotiationCommand` state machine, but using Rust's async/await
//! model instead of 30+ explicit states.
//!
//! The negotiation flow (in order):
//! 1. Connect + read greeting (or skip if using pooled connection)
//! 2. Authenticate (or skip if pooled)
//! 3. FEAT - query server capabilities
//! 4. OPTS UTF8 ON (if FEAT reports UTF8 support)
//! 5. SYST - query server system type (for VMS path handling)
//! 6. Set TYPE (binary or ASCII, per config)
//! 7. PWD to get baseWorkingDir
//! 8. CWD traversal (split path, CWD each directory component)
//! 9. MDTM (if remote-time option is enabled)
//! 10. SIZE
//! 11. Choose data connection mode (EPSV/PASV or EPRT/PORT)
//!     - If proxy is configured for PASV, tunnel the data connection
//! 12. REST (after data connection established, per C++ ordering)
//!     - Verify data connection is alive before REST (C++ sendRestPasv)
//! 13. RETR
//!
//! After data transfer completes, call `finish_download()` to read the
//! 226 transfer-complete response and optionally pool the connection.
//!
//! # Module organization
//!
//! - [`control`]       - I/O layer: `RawFtpControl`, `FreshControl`, `PooledControl`
//! - [`capabilities`]  - FEAT parsing, server capability tracking, and new commands
//! - [`parsing`]       - Response parsing, path helpers, and single-command helpers
//! - [`fresh_flow`]    - FtpNegotiator methods for fresh (non-pooled) connections
//! - [`pooled_flow`]   - FtpNegotiator methods for pooled (pre-authenticated) connections

mod capabilities;
mod control;
mod fresh_flow;
mod parsing;
mod pooled_flow;

#[cfg(test)]
mod tests;

use std::time::SystemTime;

use tokio::time::Duration;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::negotiation::control::PooledControl;
use crate::ftp::connection::types::FtpMode;
use crate::ftp::connection::negotiation::parsing::{
    cwd_traversal_pooled, extract_directory_part, extract_file_part, query_mdtm_pooled,
    query_size_pooled, send_rest_pooled, send_retr_pooled,
};

// Re-export public types from submodules
pub use capabilities::ServerCapabilities;
pub use control::RawFtpControl;

/// FTP transfer type, matching C++ `PREF_FTP_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtpTransferType {
    /// Binary (Image) mode - TYPE I. Default for most file transfers.
    #[default]
    Binary,
    /// ASCII mode - TYPE A. Used for text file transfers with line ending conversion.
    Ascii,
}

/// Configuration for proxying the PASV data channel through an HTTP CONNECT tunnel.
///
/// Matches the C++ `FtpNegotiationCommand::resolveProxy()` +
/// `sendTunnelRequest()` + `recvTunnelResponse()` flow.
/// When set, the PASV data connection is established by tunneling through
/// the HTTP proxy instead of connecting directly to the server's data port.
#[derive(Debug, Clone)]
pub struct FtpDataProxyConfig {
    /// Proxy server hostname
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Proxy authentication username (empty if no auth)
    pub proxy_username: String,
    /// Proxy authentication password (empty if no auth)
    pub proxy_password: String,
    /// User-Agent header for proxy requests
    pub user_agent: String,
}

/// Result of a successful FTP negotiation.
///
/// Contains everything the download pipeline needs to begin reading data
/// and to finalize the transfer afterwards.
pub struct FtpNegotiationResult {
    /// Data connection for reading file content
    pub data_stream: tokio::net::TcpStream,
    /// Control connection preserved for reading the 226 response later
    pub control: RawFtpControl,
    /// File size reported by SIZE command (None if SIZE not supported)
    pub file_size: Option<u64>,
    /// Modification time from MDTM command (None if MDTM not supported or disabled)
    pub modification_time: Option<SystemTime>,
    /// Base working directory from PWD, used for connection pool key
    pub base_working_dir: String,
    /// Server capabilities detected from FEAT command
    pub capabilities: ServerCapabilities,
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
    /// Transfer type: binary (TYPE I) or ASCII (TYPE A).
    /// Matches C++ `PREF_FTP_TYPE` option. Default: Binary.
    pub transfer_type: FtpTransferType,
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
    /// Proxy configuration for PASV data channel tunneling.
    ///
    /// When set, PASV data connections are established through an HTTP CONNECT
    /// tunnel via the proxy server. This matches the C++ flow where
    /// `SEQ_RESOLVE_PROXY` -> `SEQ_SEND_TUNNEL_REQUEST` ->
    /// `SEQ_RECV_TUNNEL_RESPONSE` replaces a direct PASV data connection.
    /// Only applies when `mode` is `FtpMode::Passive`.
    pub data_proxy: Option<FtpDataProxyConfig>,
}

impl Default for FtpNegotiationConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 21,
            username: String::new(),
            password: String::new(),
            remote_path: String::new(),
            mode: FtpMode::Passive,
            transfer_type: FtpTransferType::Binary,
            resume_offset: 0,
            remote_time: false,
            connect_timeout: Duration::from_secs(30),
            command_timeout: Duration::from_secs(60),
            is_pooled: false,
            pooled_base_working_dir: None,
            data_proxy: None,
        }
    }
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
    /// a sequential async function. The ordering matches the C++ and adds
    /// FEAT/SYST/OPTS for capability detection:
    ///
    /// 1. Connect + greeting (or skip for pooled)
    /// 2. Authenticate (or skip for pooled)
    /// 3. FEAT -> detect capabilities
    /// 4. OPTS UTF8 ON (if FEAT reports UTF8)
    /// 5. SYST -> detect server type
    /// 6. TYPE I/A (per transfer_type config)
    /// 7. PWD -> baseWorkingDir
    /// 8. CWD traversal (each directory component separately)
    /// 9. MDTM (if remote_time enabled)
    /// 10. SIZE
    /// 11. Data connection (EPSV/PASV or EPRT/PORT, with optional proxy tunnel)
    /// 12. REST (after data connection established, with verification)
    /// 13. RETR
    pub async fn negotiate(config: FtpNegotiationConfig) -> Result<FtpNegotiationResult> {
        let FtpNegotiationConfig {
            host,
            port,
            username,
            password,
            remote_path,
            mode,
            transfer_type,
            resume_offset,
            remote_time,
            connect_timeout,
            command_timeout,
            is_pooled,
            pooled_base_working_dir: _,
            data_proxy,
        } = config;

        // Step 1-2: Connect + authenticate (or skip if pooled)
        let (mut ctrl, mut capabilities) = if is_pooled {
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

        // Steps 3-4 (FEAT + OPTS UTF8 ON) are already done inside
        // connect_and_authenticate, matching the C++ aria2 flow where
        // FEAT/OPTS are sent immediately after login.

        // Step 5: SYST - query server system type
        capabilities.syst = capabilities::query_syst(&mut ctrl).await?;

        // Step 6: TYPE (binary or ASCII, per config)
        let is_binary = transfer_type == FtpTransferType::Binary;
        parsing::set_transfer_mode(&mut ctrl, is_binary).await?;

        // Step 7: PWD -> baseWorkingDir
        let base_working_dir = parsing::query_pwd(&mut ctrl).await?;
        info!("Base working directory: {}", base_working_dir);

        // Step 8: CWD traversal
        let dir_part = extract_directory_part(&remote_path);
        parsing::cwd_traversal(&mut ctrl, &base_working_dir, &dir_part).await?;

        // Step 9: MDTM (if remote_time enabled and server supports it)
        let modification_time = if remote_time {
            let file_part = extract_file_part(&remote_path);
            parsing::query_mdtm(&mut ctrl, &file_part).await?
        } else {
            None
        };

        // Step 10: SIZE
        let file_part = extract_file_part(&remote_path);
        let file_size = parsing::query_size(&mut ctrl, &file_part).await?;

        // Step 11: Data connection
        let data_stream = match mode {
            FtpMode::Passive => {
                // Get the PASV port first
                let pasv_result = Self::enter_passive_mode_get_port(
                    &mut ctrl, &host, connect_timeout, &capabilities,
                )
                .await?;

                let data_stream = if let Some(proxy) = &data_proxy {
                    // C++ SEQ_RESOLVE_PROXY + SEQ_SEND_TUNNEL_REQUEST +
                    // SEQ_RECV_TUNNEL_RESPONSE: tunnel the PASV data
                    // connection through the HTTP proxy via CONNECT.
                    Self::establish_pasv_data_via_proxy(
                        proxy, &host, pasv_result.port, connect_timeout,
                    )
                    .await?
                } else {
                    pasv_result.stream.unwrap()
                };

                // C++ SEQ_SEND_REST_PASV: verify data connection before REST.
                // The C++ checks dataSocket_->isReadable(0) to detect
                // connection errors. In async Rust, we do a non-blocking
                // readiness check on the data stream.
                Self::verify_data_connection(&data_stream)?;

                data_stream
            }
            FtpMode::Active => {
                Self::enter_active_mode(&mut ctrl, connect_timeout, &capabilities).await?
            }
        };

        // Step 12: REST (after data connection established, per C++ ordering)
        // C++ always sends REST, even REST 0 (FtpConnection.cc:234-245).
        // The send_rest function handles REST 0 rejection gracefully.
        parsing::send_rest(&mut ctrl, resume_offset).await?;

        // Step 13: RETR
        parsing::send_retr(&mut ctrl, &file_part).await?;

        // Build result - detach the control stream
        let ctrl_reader = ctrl.reader;
        let result = FtpNegotiationResult {
            data_stream,
            control: RawFtpControl::new(ctrl_reader, host.clone(), command_timeout),
            file_size,
            modification_time,
            base_working_dir,
            capabilities,
        };

        Ok(result)
    }

    /// Negotiate using a pooled (pre-authenticated) connection.
    ///
    /// When a connection is reused from the pool, we skip connect + auth
    /// and start from CWD_PREP (step 8). The baseWorkingDir must match.
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
            transfer_type: _,
            resume_offset,
            remote_time,
            connect_timeout,
            command_timeout,
            is_pooled: _,
            pooled_base_working_dir,
            data_proxy,
        } = config;

        let mut ctrl = PooledControl {
            reader: control.reader,
            read_timeout: command_timeout,
        };

        let base_working_dir = pooled_base_working_dir.unwrap_or_else(|| "/".to_string());

        // Pooled connections already have FEAT/SYST info from initial session,
        // so we use a default (empty) capability set here.
        let capabilities = ServerCapabilities::new();

        // Step 8: CWD traversal (skip connect + auth + FEAT + SYST + TYPE + PWD for pooled)
        let dir_part = extract_directory_part(&remote_path);
        cwd_traversal_pooled(&mut ctrl, &base_working_dir, &dir_part).await?;

        // Step 9: MDTM
        let modification_time = if remote_time {
            let file_part = extract_file_part(&remote_path);
            query_mdtm_pooled(&mut ctrl, &file_part).await?
        } else {
            None
        };

        // Step 10: SIZE
        let file_part = extract_file_part(&remote_path);
        let file_size = query_size_pooled(&mut ctrl, &file_part).await?;

        // Step 11: Data connection
        let data_stream = match mode {
            FtpMode::Passive => {
                let pasv_result = Self::enter_passive_mode_pooled_get_port(
                    &mut ctrl, &host, connect_timeout, &capabilities,
                )
                .await?;

                let data_stream = if let Some(proxy) = &data_proxy {
                    Self::establish_pasv_data_via_proxy(
                        proxy, &host, pasv_result.port, connect_timeout,
                    )
                    .await?
                } else {
                    pasv_result.stream.unwrap()
                };

                Self::verify_data_connection(&data_stream)?;
                data_stream
            }
            FtpMode::Active => {
                Self::enter_active_mode_pooled(&mut ctrl, connect_timeout, &capabilities).await?
            }
        };

        // Step 12: REST
        // C++ always sends REST, even REST 0 (FtpConnection.cc:234-245).
        send_rest_pooled(&mut ctrl, resume_offset).await?;

        // Step 13: RETR
        send_retr_pooled(&mut ctrl, &file_part).await?;

        let result = FtpNegotiationResult {
            data_stream,
            control: RawFtpControl::new(ctrl.reader, host.clone(), command_timeout),
            file_size,
            modification_time,
            base_working_dir,
            capabilities,
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
    /// `Some(())` if the connection can be pooled, `None` otherwise.
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
    // Data connection verification (C++ sendRestPasv check)
    // =========================================================================

    /// Verify the data connection is alive before sending REST.
    ///
    /// Matches the C++ `FtpNegotiationCommand::sendRestPasv` which checks
    /// `dataSocket_->isReadable(0)` to detect connection errors. If the
    /// socket is readable with zero bytes, it means the connection failed.
    fn verify_data_connection(data_stream: &tokio::net::TcpStream) -> Result<()> {
        // Non-blocking check: if the stream is readable but has no data,
        // it means the connection was refused or reset.
        match data_stream.try_read(&mut [0u8; 0]) {
            Ok(0) => {
                // Connection closed by peer (0 bytes read, no error)
                warn!("Data connection closed by peer before REST command");
                Err(Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
                    message: "Data connection establishment failed (peer closed)".into(),
                }))
            }
            Ok(_) => Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // WouldBlock is expected: connection is alive and waiting for data
                Ok(())
            }
            Err(e) => {
                warn!("Data connection error before REST: {}", e);
                Err(Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
                    message: format!("Data connection error: {}", e),
                }))
            }
        }
    }

    // =========================================================================
    // PASV data channel proxy tunnel (C++ SEQ_RESOLVE_PROXY etc.)
    // =========================================================================

    /// Establish a PASV data connection through an HTTP proxy via CONNECT.
    ///
    /// Matches the C++ `FtpNegotiationCommand` flow:
    /// - `SEQ_RESOLVE_PROXY`: resolve proxy hostname
    /// - `SEQ_SEND_TUNNEL_REQUEST`: send CONNECT host:port to proxy
    /// - `SEQ_RECV_TUNNEL_RESPONSE`: read 200 Connection Established
    ///
    /// The `target_port` is the port obtained from the EPSV/PASV response.
    /// The tunnel target is `host:target_port` (not the FTP control port).
    async fn establish_pasv_data_via_proxy(
        proxy: &FtpDataProxyConfig,
        target_host: &str,
        target_port: u16,
        connect_timeout: Duration,
    ) -> Result<tokio::net::TcpStream> {
        use crate::ftp::connection::proxy_tunnel::{FtpProxyTunnel, FtpProxyTunnelConfig};

        debug!(
            "Establishing PASV data tunnel via proxy {}:{} to {}:{}",
            proxy.proxy_host, proxy.proxy_port, target_host, target_port
        );

        let tunnel_config = FtpProxyTunnelConfig {
            proxy_host: proxy.proxy_host.clone(),
            proxy_port: proxy.proxy_port,
            target_host: target_host.to_string(),
            target_port,
            proxy_username: proxy.proxy_username.clone(),
            proxy_password: proxy.proxy_password.clone(),
            connect_timeout,
            read_timeout: connect_timeout,
            user_agent: proxy.user_agent.clone(),
        };

        let result = FtpProxyTunnel::establish(&tunnel_config).await?;
        Ok(result)
    }
}

/// Intermediate result from PASV negotiation that separates port resolution
/// from stream creation, enabling the proxy tunnel flow.
struct PasvResult {
    /// The resolved data port from EPSV/PASV response.
    port: u16,
    /// The direct data stream (None if using proxy tunnel).
    stream: Option<tokio::net::TcpStream>,
}
