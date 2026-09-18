//! Public FTP negotiation data types.

use std::time::{Duration, SystemTime};

use super::capabilities::ServerCapabilities;
use crate::ftp::connection::negotiation::control::RawFtpControl;

/// FTP data connection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtpMode {
    /// Passive mode (the client connects to the server's data port).
    #[default]
    Passive,
    /// Active mode (the server connects to the client's listener).
    Active,
}

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
    /// Base working directory for pooled connections (must match).
    ///
    /// This field is read only by [`FtpNegotiator::negotiate_pooled`]; fresh
    /// negotiation ignores it.
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
            pooled_base_working_dir: None,
            data_proxy: None,
        }
    }
}

/// Public standalone FTP negotiation orchestrator.
///
/// Performs the full FTP negotiation flow as a linear async function instead
/// of the C++ state machine with 30+ states. Use [`Self::negotiate`] for a
/// fresh connection and [`Self::negotiate_pooled`] when a pre-authenticated
/// control stream is already available; the distinction is expressed by the
/// method Interface rather than a boolean configuration flag.
pub struct FtpNegotiator;

/// Intermediate result from PASV negotiation that separates port resolution
/// from stream creation, enabling the proxy tunnel flow.
pub(super) struct PasvResult {
    /// The resolved data port from EPSV/PASV response.
    pub(super) port: u16,
    /// The direct data stream (None if using proxy tunnel).
    pub(super) stream: Option<tokio::net::TcpStream>,
}
