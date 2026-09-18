//! FTP types and data structures
//!
//! Defines the core types used throughout the FTP client implementation:
//! connection mode, TLS mode, server response, file metadata, and the client struct.

use tokio::time::Duration;

use aria2_protocol::ftp::tls::{FtpControlStream, FtpsConfig};

/// FTP data connection mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtpMode {
    /// Passive mode (client connects to server data port)
    #[default]
    Passive,
    /// Active mode (server connects to client data port)
    Active,
}

/// FTPS (FTP over TLS) connection mode per RFC 4217.
///
/// Controls when and how TLS is applied to the FTP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtpTlsMode {
    /// No TLS — plain FTP on the standard control/data channels.
    #[default]
    None,
    /// Explicit FTPS — connect in plaintext, then upgrade via AUTH TLS.
    ///
    /// After the 220 greeting, the client sends `AUTH TLS`, receives 234,
    /// and upgrades the control stream to TLS. Data connections are also
    /// upgraded if `PROT P` was negotiated.
    ///
    /// This is the most common FTPS mode (RFC 4217 section 3).
    Explicit,
    /// Implicit FTPS — TLS from the very start (typically on port 990).
    ///
    /// The TCP connection is immediately wrapped in TLS before any FTP
    /// protocol exchange. No `AUTH TLS` command is sent. This is the
    /// legacy FTPS mode predating RFC 4217.
    Implicit,
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

/// Legacy standalone FTP client adapter.
///
/// This type is kept for callers that need the original high-level client
/// surface. The production download engine uses [`super::FtpControlStream`],
/// [`super::FtpDataStream`], and [`super::FtpNegotiator`] directly instead of
/// this adapter.
///
/// The adapter supports:
/// - Passive mode priority, with active mode fallback
/// - Binary/ASCII transfer mode switching
/// - Resume transfer (REST command)
/// - Directory listing parsing (Unix/Windows format)
/// - FTPS (FTP over TLS) with explicit (AUTH TLS) and implicit modes
///
/// # FTPS data-channel limitation
///
/// [`FtpClient::passive_mode`] and [`FtpClient::active_mode`] return plain
/// [`std::net::TcpStream`] values. They cannot expose the TLS-wrapped data
/// stream required by `PROT P`; use [`super::FtpNegotiator`] for protected
/// FTPS data transfers.
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
    /// Control connection stream (buffered, polymorphic over plain/TLS).
    ///
    /// For plain FTP, this wraps `FtpControlStream::Plain(TcpStream)`.
    /// For FTPS, this wraps `FtpControlStream::Tls(TlsStream<TcpStream>)`.
    pub(crate) control_stream: tokio::io::BufReader<FtpControlStream>,
    /// Data connection mode
    pub(crate) mode: FtpMode,
    /// Current binary mode state
    pub(crate) binary_mode: bool,
    /// Server host address
    pub(crate) host: String,
    /// Control connection's selected peer address. Passive data channels
    /// must use this address rather than trusting a PASV response host.
    pub(crate) control_peer: std::net::IpAddr,
    /// Server port
    #[allow(dead_code)] // Port field retained for FTP connection configuration
    pub(crate) port: u16,
    /// Connection timeout
    pub(crate) connect_timeout: Duration,
    /// Read timeout
    pub(crate) read_timeout: Duration,
    /// Base working directory obtained from PWD after login.
    /// Used for connection pooling: a pooled connection must have
    /// the same base_working_dir to be reused for the same host.
    pub(crate) base_working_dir: String,
    /// Server features detected from FEAT command response.
    /// Populated after `send_feat()` is called; None until then.
    pub(crate) features: Option<super::feat::FtpFeatures>,
    /// FTPS TLS mode for this connection.
    /// When set to Explicit or Implicit, data connections will also
    /// be upgraded to TLS after PROT P is negotiated.
    pub(crate) tls_mode: FtpTlsMode,
    /// Whether PROT P (Private data channel protection) has been negotiated.
    /// When true, data connections must also be upgraded to TLS.
    pub(crate) data_channel_protected: bool,
    /// FTPS TLS configuration. Used to build TLS connectors for both
    /// control and data channel upgrades.
    pub(crate) ftps_config: Option<FtpsConfig>,
}

// Re-export FtpFeatures from the feat module so public API stays accessible
// via `ftp::connection::FtpFeatures`.
pub use super::feat::FtpFeatures;
