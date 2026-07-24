//! FTP types and data structures
//!
//! Defines the core types used throughout the FTP client implementation:
//! connection mode, server response, file metadata, and the client struct.

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::time::Duration;

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
    /// Base working directory obtained from PWD after login.
    /// Used for connection pooling: a pooled connection must have
    /// the same base_working_dir to be reused for the same host.
    pub(crate) base_working_dir: String,
}
