//! SFTP Session Management
//!
//! Manages the SFTP protocol session lifecycle including version negotiation,
//! request ID tracking, and operation timeout handling. Built entirely on
//! pure Rust using `russh` channels and the packet codec.
//!
//! ## Session Lifecycle
//!
//! ```text
//! SSH Connection  ->  Open SFTP Channel  ->  Send INIT  ->  Receive VERSION
//!       |                  |                    |              |
//!       v                  v                    v              v
//!   Authenticated     russh::Channel      Request ID=0   Server Version + Extensions
//!                                                    |
//!                                                    v
//!                                             Ready for file operations
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::connection::SshConnection;
use super::packet::SftpPacket;

mod error;
#[cfg(test)]
mod tests;

pub use error::SftpSessionError;

/// Minimum supported SFTP protocol version
pub const SFTP_VERSION_MIN: u32 = 3;
/// Maximum supported SFTP protocol version
pub const SFTP_VERSION_MAX: u32 = 6;
/// Represents a server-side SFTP protocol extension
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpExtension {
    /// Extension name (e.g., "hardlink@openssh.com")
    pub name: String,
    /// Extension-specific data (often version number)
    pub data: String,
}

/// Serialized I/O state for one SFTP subsystem channel.
///
/// `russh::Channel` already owns the receive half for its channel. Keeping
/// that stream and the packet buffer together prevents a second forwarding
/// queue, unnecessary packet copies, and response-order races.
struct SftpIo {
    stream: russh::ChannelStream<russh::client::Msg>,
    recv_buffer: Vec<u8>,
}

impl std::fmt::Display for SftpExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.name, self.data)
    }
}

/// Manages an active SFTP session over a russh SSH channel.
///
/// This struct handles:
/// - SFTP version negotiation during initialization (using the packet codec)
/// - Monotonically increasing request IDs for request/response correlation
/// - Timeout tracking for in-flight operations
/// - Server extension advertisement tracking
/// - Packet I/O through the russh channel using packet encoding/decoding
///
/// **No unsafe code** -- all memory management is handled by Rust's ownership system.
pub struct SftpSession {
    /// Serialized SFTP channel I/O and packet reassembly state.
    io: Arc<Mutex<SftpIo>>,
    /// The negotiated server SFTP protocol version
    server_version: u32,
    /// Monotonically increasing request ID counter
    next_request_id: Arc<AtomicU32>,
    /// Server-advertised extensions from the VERSION response
    extensions: Vec<SftpExtension>,
    /// When this session was initialized
    created_at: std::time::Instant,
    /// Completed request/response exchanges (for metrics/diagnostics)
    operation_count: Arc<AtomicU32>,
    /// Read timeout for individual operations
    read_timeout: Duration,
}

impl std::fmt::Debug for SftpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpSession")
            .field("server_version", &self.server_version)
            .field("extensions", &self.extensions)
            .field("age_secs", &self.age().as_secs())
            .finish()
    }
}

/// Clone implementation for SftpSession -- all mutable state is shared, so
/// request IDs and metrics remain correct across clones.
impl Clone for SftpSession {
    fn clone(&self) -> Self {
        Self {
            io: Arc::clone(&self.io),
            server_version: self.server_version,
            next_request_id: Arc::clone(&self.next_request_id),
            extensions: self.extensions.clone(),
            created_at: self.created_at,
            operation_count: Arc::clone(&self.operation_count),
            read_timeout: self.read_timeout,
        }
    }
}

impl SftpSession {
    /// Initialize an SFTP session over an established SSH connection.
    ///
    /// This method:
    /// 1. Opens an SFTP subsystem channel via the SSH connection
    /// 2. Sends SSH_FXP_INIT(version=3) using packet encoding
    /// 3. Receives SSH_FXP_VERSION response via channel data callback
    /// 4. Parses the response using packet decoding
    /// 5. Extracts server version and advertised extensions
    ///
    /// # Arguments
    /// * `conn` - An authenticated SSH connection ready for subsystem use
    ///
    /// # Returns
    /// A fully initialized `SftpSession` ready for file operations.
    pub async fn open(conn: &mut SshConnection) -> Result<Self, SftpSessionError> {
        debug!("[SFTP] Initializing SFTP session (pure Rust)...");

        // Step 1: Open SFTP subsystem channel on the connection
        let channel = conn.open_sftp_channel().await?;

        let read_timeout = conn.options().read_timeout;

        let mut io = SftpIo {
            stream: channel.into_stream(),
            recv_buffer: Vec::new(),
        };

        // Step 2: Send SSH_FXP_INIT(version=3) using the packet codec
        let init_pkt = SftpPacket::Init { version: 3 };
        let encoded = init_pkt.encode().map_err(SftpSessionError::PacketEncode)?;

        Self::write_packet(&mut io, &encoded, "SFTP INIT").await?;

        debug!("[SFTP] Sent SFTP INIT (version=3), awaiting VERSION...");

        // Step 3: Receive SSH_FXP_VERSION response with timeout
        let version_response = Self::recv_packet_from_io(&mut io, read_timeout).await?;

        // Step 4: Parse the VERSION response
        let (server_version, extensions) = match version_response {
            SftpPacket::Version {
                version,
                extensions,
            } => {
                if version < SFTP_VERSION_MIN {
                    return Err(SftpSessionError::UnsupportedVersion {
                        server_version: version,
                        minimum: SFTP_VERSION_MIN,
                    });
                }
                (version, extensions)
            }
            other => {
                return Err(SftpSessionError::UnexpectedPacket {
                    expected: "VERSION",
                    actual_type: other.packet_type(),
                });
            }
        };

        info!(
            "[SFTP] Session established (server version=v{}, supports v{}-v{}, {} extensions)",
            server_version,
            SFTP_VERSION_MIN,
            SFTP_VERSION_MAX,
            extensions.len()
        );

        Ok(Self {
            io: Arc::new(Mutex::new(io)),
            server_version,
            next_request_id: Arc::new(AtomicU32::new(1)), // Start at 1, 0 reserved for INIT
            extensions: extensions
                .into_iter()
                .map(|(name, data)| SftpExtension { name, data })
                .collect(),
            created_at: std::time::Instant::now(),
            operation_count: Arc::new(AtomicU32::new(0)),
            read_timeout,
        })
    }

    async fn write_packet(
        io: &mut SftpIo,
        encoded: &[u8],
        operation: &'static str,
    ) -> Result<(), SftpSessionError> {
        io.stream
            .write_all(encoded)
            .await
            .map_err(|source| SftpSessionError::ChannelWrite { operation, source })?;
        io.stream
            .flush()
            .await
            .map_err(|source| SftpSessionError::ChannelFlush { operation, source })
    }

    /// Internal implementation of packet reception with buffering.
    ///
    /// SFTP packets may arrive fragmented across multiple channel data callbacks.
    /// This method buffers incoming data until a complete packet (with length prefix)
    /// can be decoded.
    async fn recv_packet_from_io(
        io: &mut SftpIo,
        timeout: Duration,
    ) -> Result<SftpPacket, SftpSessionError> {
        loop {
            // Try to decode a complete packet from the buffer first
            if !io.recv_buffer.is_empty() {
                match SftpPacket::decode(io.recv_buffer.as_slice()) {
                    Ok((pkt, consumed)) => {
                        let remaining = io.recv_buffer.split_off(consumed);
                        io.recv_buffer = remaining;
                        return Ok(pkt);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
                    Err(error) => return Err(SftpSessionError::PacketDecode(error)),
                }
            }

            let mut chunk = [0_u8; 32 * 1024];
            let count = tokio::time::timeout(timeout, io.stream.read(&mut chunk))
                .await
                .map_err(|_| SftpSessionError::Timeout(timeout))?
                .map_err(SftpSessionError::ChannelRead)?;
            if count == 0 {
                return Err(SftpSessionError::ChannelClosed);
            }
            io.recv_buffer.extend_from_slice(&chunk[..count]);
        }
    }

    /// Combined send + receive with automatic request ID management.
    ///
    /// This is the only public request path. It allocates a request ID, sets it
    /// in the packet, sends it, verifies the response ID, and records the
    /// completed exchange.
    pub(super) async fn request(
        &self,
        mut pkt: SftpPacket,
    ) -> Result<SftpPacket, SftpSessionError> {
        let req_id = self.allocate_request_id();

        // Set the request ID in the packet
        Self::set_request_id_in_packet(&mut pkt, req_id);

        // Keep one request/response exchange serialized. SFTP servers can
        // reply out of order, while this client intentionally exposes a
        // synchronous operation seam rather than a speculative pending map.
        let encoded = pkt.encode().map_err(SftpSessionError::PacketEncode)?;
        let mut io = self.io.lock().await;
        Self::write_packet(&mut io, &encoded, "SFTP request").await?;
        let response = Self::recv_packet_from_io(&mut io, self.read_timeout).await?;

        let actual_id = response.request_id();
        if actual_id != Some(req_id) {
            return Err(SftpSessionError::RequestIdMismatch {
                expected: req_id,
                actual: actual_id,
            });
        }

        self.operation_count.fetch_add(1, Ordering::Relaxed);
        Ok(response)
    }

    /// Set the request ID field in any request-type SftpPacket.
    fn set_request_id_in_packet(pkt: &mut SftpPacket, new_request_id: u32) {
        match pkt {
            SftpPacket::Open { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Close { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Read { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Write { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Stat { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Lstat { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Fstat { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Setstat { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Fsetstat { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Opendir { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Readdir { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Realpath { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Remove { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Mkdir { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Rmdir { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Rename { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Readlink { request_id, .. } => *request_id = new_request_id,
            SftpPacket::Symlink { request_id, .. } => *request_id = new_request_id,
            // Init/Version have no request_id; Status/Handle/Data/Name/Attrs are responses
            _ => {} // No-op for non-request or response packets
        }
    }

    /// Get the negotiated server SFTP protocol version
    pub fn server_version(&self) -> u32 {
        self.server_version
    }

    /// Get how long this session has been active
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Get the number of completed request/response exchanges in this session.
    pub fn operation_count(&self) -> u32 {
        self.operation_count.load(Ordering::Relaxed)
    }

    /// Get the configured read timeout for operations
    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    // ====================================================================
    // Request ID Management
    // ====================================================================

    /// Allocate the next unique request ID.
    ///
    /// Request IDs are monotonically increasing and used to correlate
    /// requests with their responses. The SFTP protocol requires each
    /// request to carry a unique ID that the server echoes back.
    ///
    /// # Returns
    /// A new unique request ID (never zero)
    fn allocate_request_id(&self) -> u32 {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        // Wrap around check: if we've exhausted u32 space, warn but continue
        // In practice this won't happen (4 billion requests per session)
        if id == 0 {
            warn!("[SFTP] Request ID counter wrapped around");
            self.next_request_id.store(1, Ordering::Relaxed);
            1
        } else {
            id
        }
    }

    // ====================================================================
    // Extension Support
    // ====================================================================

    /// Check if the server advertises a specific extension by name.
    ///
    /// Common extensions include:
    /// - `hardlink@openssh.com` - Hard link creation
    /// - `fsync@openssh.com` - File sync to disk
    /// - `posix-rename@openssh.com` - POSIX-compliant rename (atomic)
    /// - `statvfs@openssh.com` - Filesystem statistics
    /// - `fstatvfs@openssh.com` - Filesystem statistics by handle
    /// - `lsetstat@openssh.com` - Set attributes without following symlinks
    /// - `limits@openssh.com` - Server-side limits information
    /// - `expand-path@openssh.com` - Path expansion with tilde/env vars
    /// - `copy-data` - Server-side file copy (draft)
    /// - `space-available` - Available space query (draft)
    /// - `open@openssh.com` - Extended open flags
    /// - `close@openssh.com` - Extended close with reasons
    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|ext| ext.name == name)
    }

    /// Get extension data by name, if present.
    pub fn get_extension(&self, name: &str) -> Option<&SftpExtension> {
        self.extensions.iter().find(|ext| ext.name == name)
    }

    /// Get all extensions advertised by the server.
    pub fn extensions(&self) -> &[SftpExtension] {
        &self.extensions
    }

    // ====================================================================
    // Diagnostics
    // ====================================================================

    /// Generate a diagnostic summary of this session's state.
    pub fn diagnostics(&self) -> String {
        format!(
            "SftpSession{{version=v{}, age={}s, ops={}, extensions=[{}]}}",
            self.server_version,
            self.age().as_secs(),
            self.operation_count(),
            self.extensions
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

// =============================================================================
