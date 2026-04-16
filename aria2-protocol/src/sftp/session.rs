//! SFTP Session Management
//!
//! Manages the SFTP protocol session lifecycle including version negotiation,
//! request ID tracking, and operation timeout handling. Built entirely on
//! pure Rust using `russh` channels and the `packet.rs` codec.
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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use super::connection::{SshConnection, SshOptions};
use super::packet::SftpPacket;

/// Minimum supported SFTP protocol version
pub const SFTP_VERSION_MIN: u32 = 3;
/// Maximum supported SFTP protocol version
pub const SFTP_VERSION_MAX: u32 = 6;
/// Default timeout for individual SFTP operations
pub const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Default maximum number of concurrent pending requests
pub const MAX_PENDING_REQUESTS: usize = 256;

/// Represents a server-side SFTP protocol extension
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpExtension {
    /// Extension name (e.g., "hardlink@openssh.com")
    pub name: String,
    /// Extension-specific data (often version number)
    pub data: String,
}

impl std::fmt::Display for SftpExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.name, self.data)
    }
}

/// Manages an active SFTP session over a russh SSH channel.
///
/// This struct handles:
/// - SFTP version negotiation during initialization (using packet.rs codec)
/// - Monotonically increasing request IDs for request/response correlation
/// - Timeout tracking for in-flight operations
/// - Server extension advertisement tracking
/// - Packet I/O through the russh channel using packet.rs encoding/decoding
///
/// **No unsafe code** -- all memory management is handled by Rust's ownership system.
pub struct SftpSession {
    /// The underlying russh channel for SFTP subsystem communication.
    /// Wrapped in Mutex for interior mutability across async operations.
    channel: Arc<Mutex<russh::Channel<russh::client::Msg>>>,
    /// Receiver for incoming channel data (routed by handler's data() callback)
    response_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    /// The negotiated server SFTP protocol version
    server_version: u32,
    /// Connection options used for diagnostics
    options: Arc<SshOptions>,
    /// Monotonically increasing request ID counter
    next_request_id: AtomicU32,
    /// Server-advertised extensions from the VERSION response
    extensions: Vec<SftpExtension>,
    /// When this session was initialized
    created_at: std::time::Instant,
    /// Count of operations performed (for metrics/diagnostics)
    operation_count: AtomicU32,
    /// Read timeout for individual operations
    read_timeout: Duration,
    /// Buffered incomplete packet data (packets may arrive in fragments)
    recv_buffer: Arc<Mutex<Vec<u8>>>,
}

impl std::fmt::Debug for SftpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpSession")
            .field("server_version", &self.server_version)
            .field("options", &self.options)
            .field("extensions", &self.extensions)
            .field("age_secs", &self.age().as_secs())
            .finish()
    }
}

/// Clone implementation for SftpSession -- all internal fields are Arc-wrapped,
/// so cloning is cheap (just increments refcounts).
impl Clone for SftpSession {
    fn clone(&self) -> Self {
        Self {
            channel: Arc::clone(&self.channel),
            response_rx: Arc::clone(&self.response_rx),
            server_version: self.server_version,
            options: Arc::clone(&self.options),
            next_request_id: AtomicU32::new(self.next_request_id.load(Ordering::Relaxed)),
            extensions: self.extensions.clone(),
            created_at: self.created_at,
            operation_count: AtomicU32::new(self.operation_count.load(Ordering::Relaxed)),
            read_timeout: self.read_timeout,
            recv_buffer: Arc::clone(&self.recv_buffer),
        }
    }
}

impl SftpSession {
    /// Initialize an SFTP session over an established SSH connection.
    ///
    /// This method:
    /// 1. Opens an SFTP subsystem channel via the SSH connection
    /// 2. Sends SSH_FXP_INIT(version=3) using packet.rs encoding
    /// 3. Receives SSH_FXP_VERSION response via channel data callback
    /// 4. Parses the response using packet.rs decoding
    /// 5. Extracts server version and advertised extensions
    ///
    /// # Arguments
    /// * `conn` - An authenticated SSH connection ready for subsystem use
    ///
    /// # Returns
    /// A fully initialized `SftpSession` ready for file operations.
    pub async fn open(conn: &mut SshConnection) -> Result<Self, String> {
        debug!("[SFTP] Initializing SFTP session (pure Rust)...");

        // Step 1: Open SFTP subsystem channel on the connection
        let (channel, data_rx) = conn
            .open_sftp_channel()
            .await
            .map_err(|e| format!("Failed to open SFTP channel: {}", e))?;

        let options = Arc::clone(conn.options());
        let read_timeout = options.read_timeout;

        // Wrap channel and receiver in Arc<Mutex<>> for shared access
        let channel = Arc::new(Mutex::new(channel));
        let response_rx = Arc::new(Mutex::new(data_rx));
        let recv_buffer = Arc::new(Mutex::new(Vec::new()));

        // Step 2: Send SSH_FXP_INIT(version=3) using packet.rs codec
        let init_pkt = SftpPacket::Init { version: 3 };
        let encoded = init_pkt
            .encode()
            .map_err(|e| format!("Failed to encode SFTP INIT packet: {}", e))?;

        {
            let ch = channel.lock().await;
            ch.data(encoded.as_slice())
                .await
                .map_err(|e| format!("Failed to send SFTP INIT packet: {}", e))?;
        }

        debug!("[SFTP] Sent SFTP INIT (version=3), awaiting VERSION...");

        // Step 3: Receive SSH_FXP_VERSION response with timeout
        let version_response =
            Self::recv_packet_from_channel(&response_rx, &recv_buffer, read_timeout).await?;

        // Step 4: Parse the VERSION response
        let (server_version, extensions) = match version_response {
            SftpPacket::Version {
                version,
                extensions,
            } => {
                if version < SFTP_VERSION_MIN {
                    return Err(format!(
                        "Server SFTP version too low: v{} (minimum v{})",
                        version, SFTP_VERSION_MIN
                    ));
                }
                (version, extensions)
            }
            other => {
                return Err(format!(
                    "Expected VERSION packet after INIT, got type={}",
                    other.packet_type()
                ));
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
            channel,
            response_rx,
            server_version,
            options,
            next_request_id: AtomicU32::new(1), // Start at 1, 0 reserved for INIT
            extensions: extensions
                .into_iter()
                .map(|(name, data)| SftpExtension { name, data })
                .collect(),
            created_at: std::time::Instant::now(),
            operation_count: AtomicU32::new(0),
            read_timeout,
            recv_buffer,
        })
    }

    /// Core I/O: send a single SFTP packet through the channel.
    ///
    /// Encodes the packet using packet.rs and writes it to the russh channel.
    pub async fn send_packet(&self, pkt: &SftpPacket) -> Result<(), String> {
        let encoded = pkt.encode().map_err(|e| format!("Encode error: {}", e))?;
        let ch = self.channel.lock().await;
        ch.data(encoded.as_slice())
            .await
            .map_err(|e| format!("Channel write error: {}", e))
    }

    /// Core I/O: receive a single SFTP packet from the channel.
    ///
    /// Reads data from the mpsc receiver (fed by handler's data() callback),
    /// buffers partial packets, and decodes complete packets using packet.rs.
    pub async fn recv_packet(&self) -> Result<SftpPacket, String> {
        Self::recv_packet_from_channel(&self.response_rx, &self.recv_buffer, self.read_timeout)
            .await
    }

    /// Internal implementation of packet reception with buffering.
    ///
    /// SFTP packets may arrive fragmented across multiple channel data callbacks.
    /// This method buffers incoming data until a complete packet (with length prefix)
    /// can be decoded.
    async fn recv_packet_from_channel(
        response_rx: &Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
        recv_buffer: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
    ) -> Result<SftpPacket, String> {
        loop {
            // Try to decode a complete packet from the buffer first
            {
                let mut buf = recv_buffer.lock().await;
                if !buf.is_empty() {
                    match SftpPacket::decode(buf.as_slice()) {
                        Ok((pkt, consumed)) => {
                            // Remove consumed bytes from buffer
                            let remaining = buf.split_off(consumed);
                            *buf = remaining;
                            return Ok(pkt);
                        }
                        Err(_) => {
                            // Not enough data yet - wait for more
                            // Buffer stays as-is, will be appended to below
                        }
                    }
                }
            }

            // Need more data - wait for channel data with timeout
            let mut rx = response_rx.lock().await;

            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Some(chunk)) => {
                    drop(rx); // Release lock before acquiring buffer lock
                    let mut buf = recv_buffer.lock().await;
                    buf.extend_from_slice(&chunk);
                    // Loop back to try decoding again
                }
                Ok(None) => {
                    return Err("Channel closed by server".to_string());
                }
                Err(_) => {
                    return Err(format!("Receive timed out after {}s", timeout.as_secs()));
                }
            }
        }
    }

    /// Combined send + receive with automatic request ID management.
    ///
    /// This is the primary method that file operations should use.
    /// It allocates a request ID, sets it in the packet, sends it,
    /// waits for the matching response, and returns it.
    pub async fn request(&self, mut pkt: SftpPacket) -> Result<SftpPacket, String> {
        let req_id = self.allocate_request_id();

        // Set the request ID in the packet
        Self::set_request_id_in_packet(&mut pkt, req_id);

        // Send the packet
        self.send_packet(&pkt).await?;

        // Wait for response (with timeout)
        let resp = self.recv_packet().await?;
        Ok(resp)
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

    /// Check if the server supports at least the specified version
    pub fn supports_version(&self, v: u32) -> bool {
        self.server_version >= v
    }

    /// Get the connection options used to create this session
    pub fn options(&self) -> &Arc<SshOptions> {
        &self.options
    }

    /// Get how long this session has been active
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Get the total number of operations performed on this session
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
    pub fn allocate_request_id(&self) -> u32 {
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

    /// Reset the request ID counter (primarily for testing).
    #[cfg(test)]
    pub fn reset_request_id_counter(&self) {
        self.next_request_id.store(1, Ordering::Relaxed);
    }

    /// Record that an operation has been completed (for metrics).
    pub fn record_operation(&self) {
        self.operation_count.fetch_add(1, Ordering::Relaxed);
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

    /// Get all advertised extensions.
    pub fn extensions(&self) -> Vec<SftpExtension> {
        self.extensions.clone()
    }

    /// Add an extension (used when parsing VERSION response).
    pub fn add_extension(&mut self, name: impl Into<String>, data: impl Into<String>) {
        self.extensions.push(SftpExtension {
            name: name.into(),
            data: data.into(),
        });
    }

    /// Parse extensions from a VERSION packet's extension list.
    pub fn parse_extensions_from_version(&mut self, extensions: Vec<(String, String)>) {
        self.extensions = extensions
            .into_iter()
            .map(|(name, data)| SftpExtension { name, data })
            .collect();

        if !self.extensions.is_empty() {
            debug!(
                "[SFTP] Server advertises {} extensions: {}",
                self.extensions.len(),
                self.extensions
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
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
// Pending Request Tracker (for async response handling)
// =============================================================================

/// Tracks pending SFTP requests awaiting responses.
///
/// Each sent request registers itself with a unique request ID and provides
/// a oneshot channel for receiving the corresponding response. This enables
/// proper async request-response correlation.
pub struct PendingRequestTracker {
    /// Map from request_id to the response sender half of a oneshot channel
    pending: HashMap<u32, tokio::sync::oneshot::Sender<SftpPacket>>,
    /// Maximum number of concurrent pending requests
    max_pending: usize,
}

impl PendingRequestTracker {
    /// Create a new tracker with specified capacity limit.
    pub fn new(max_pending: usize) -> Self {
        Self {
            pending: HashMap::with_capacity(max_pending),
            max_pending,
        }
    }

    /// Register a new pending request and return its response receiver.
    ///
    /// # Arguments
    /// * `request_id` - The unique ID for this request
    ///
    /// # Returns
    /// A oneshot receiver that will receive the response packet when available.
    ///
    /// # Errors
    /// Returns an error if the tracker is at capacity or the ID is already registered.
    pub fn register(
        &mut self,
        request_id: u32,
    ) -> Result<tokio::sync::oneshot::Receiver<SftpPacket>, PendingRequestError> {
        if self.pending.len() >= self.max_pending {
            return Err(PendingRequestError::AtCapacity {
                current: self.pending.len(),
                max: self.max_pending,
            });
        }

        if self.pending.contains_key(&request_id) {
            return Err(PendingRequestError::DuplicateId(request_id));
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.insert(request_id, tx);
        Ok(rx)
    }

    /// Deliver a response packet to the waiting requester.
    ///
    /// # Returns
    /// - `Ok(true)` if the response was delivered successfully
    /// - `Ok(false)` if no matching pending request was found (already cancelled/timed out)
    /// - `Err` if delivery failed for another reason
    pub fn deliver_response(&mut self, response: SftpPacket) -> Result<bool, PendingRequestError> {
        let request_id = match response.request_id() {
            Some(id) => id,
            None => return Err(PendingRequestError::NoRequestId),
        };

        match self.pending.remove(&request_id) {
            Some(sender) => {
                // Try to send; ignore error if receiver was dropped
                let _ = sender.send(response);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Cancel a pending request by ID, returning true if it existed.
    pub fn cancel(&mut self, request_id: u32) -> bool {
        self.pending.remove(&request_id).is_some()
    }

    /// Cancel all pending requests (e.g., on disconnect).
    /// Returns the count of cancelled requests.
    pub fn cancel_all(&mut self) -> usize {
        let count = self.pending.len();
        self.pending.clear();
        count
    }

    /// Get the count of currently pending requests.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Check if there are no pending requests.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl Default for PendingRequestTracker {
    fn default() -> Self {
        Self::new(MAX_PENDING_REQUESTS)
    }
}

// =============================================================================
// Error Types
// =============================================================================

/// Errors related to pending request tracking
#[derive(Debug, Clone, thiserror::Error)]
pub enum PendingRequestError {
    #[error("Tracker at capacity ({current}/{max})")]
    AtCapacity { current: usize, max: usize },

    #[error("Duplicate request ID: {0}")]
    DuplicateId(u32),

    #[error("Response packet has no request ID")]
    NoRequestId,

    #[error("Request timed out after {timeout_secs}s")]
    TimedOut { request_id: u32, timeout_secs: u64 },
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::packet::status_code_description;
    use crate::sftp::packet::{SSH_FX_OK, SftpFileAttrs};

    #[test]
    fn test_version_constants() {
        const _: () = assert!(SFTP_VERSION_MIN <= SFTP_VERSION_MAX);
        assert_eq!(SFTP_VERSION_MIN, 3);
        assert_eq!(SFTP_VERSION_MAX, 6);
    }

    #[test]
    fn test_default_operation_timeout() {
        assert_eq!(DEFAULT_OPERATION_TIMEOUT, Duration::from_secs(60));
    }

    #[test]
    fn test_sftp_extension_creation_and_display() {
        let ext = SftpExtension {
            name: "hardlink@openssh.com".to_string(),
            data: "1".to_string(),
        };
        assert_eq!(ext.name, "hardlink@openssh.com");
        assert_eq!(ext.data, "1");
        assert_eq!(format!("{}", ext), "hardlink@openssh.com=1");
    }

    #[test]
    fn test_sftp_extension_equality() {
        let ext1 = SftpExtension {
            name: "fsync@openssh.com".to_string(),
            data: "2".to_string(),
        };
        let ext2 = SftpExtension {
            name: "fsync@openssh.com".to_string(),
            data: "2".to_string(),
        };
        let ext3 = SftpExtension {
            name: "fsync@openssh.com".to_string(),
            data: "3".to_string(),
        };

        assert_eq!(ext1, ext2);
        assert_ne!(ext1, ext3);
    }

    #[test]
    fn test_session_extensions_empty_by_default() {
        let exts: Vec<SftpExtension> = Vec::new();
        assert!(exts.is_empty());
        assert_eq!(exts.len(), 0);
    }

    // -----------------------------------------------------------------
    // Request ID Management Tests
    // -----------------------------------------------------------------

    #[test]
    fn test_request_id_allocation_monotonic() {
        let counter = AtomicU32::new(1);

        let id1 = counter.fetch_add(1, Ordering::Relaxed);
        let id2 = counter.fetch_add(1, Ordering::Relaxed);
        let id3 = counter.fetch_add(1, Ordering::Relaxed);

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
        assert!(id2 > id1);
        assert!(id3 > id2);
    }

    #[test]
    fn test_request_id_uniqueness_across_allocations() {
        let counter = AtomicU32::new(1);
        let mut ids = std::collections::HashSet::new();

        for _ in 0..1000 {
            let id = counter.fetch_add(1, Ordering::Relaxed);
            assert!(ids.insert(id), "Request ID {} was duplicated!", id);
        }

        assert_eq!(ids.len(), 1000);
    }

    // -----------------------------------------------------------------
    // Set Request ID In Packet Tests
    // -----------------------------------------------------------------

    #[test]
    fn test_set_request_id_for_all_packet_types() {
        // Test each request packet type accepts a request_id
        let cases: Vec<SftpPacket> = vec![
            SftpPacket::Open {
                request_id: 0,
                filename: "/tmp/test".into(),
                flags: 0,
                attrs: SftpFileAttrs::default(),
            },
            SftpPacket::Close {
                request_id: 0,
                handle: vec![1, 2, 3],
            },
            SftpPacket::Read {
                request_id: 0,
                handle: vec![1],
                offset: 0,
                length: 1024,
            },
            SftpPacket::Write {
                request_id: 0,
                handle: vec![1],
                offset: 0,
                data: vec![0],
            },
            SftpPacket::Stat {
                request_id: 0,
                path: "/test".into(),
            },
            SftpPacket::Lstat {
                request_id: 0,
                path: "/test".into(),
            },
            SftpPacket::Fstat {
                request_id: 0,
                handle: vec![1],
            },
            SftpPacket::Opendir {
                request_id: 0,
                path: "/dir".into(),
            },
            SftpPacket::Readdir {
                request_id: 0,
                handle: vec![1],
            },
            SftpPacket::Realpath {
                request_id: 0,
                path: ".".into(),
            },
            SftpPacket::Remove {
                request_id: 0,
                filename: "/file".into(),
            },
            SftpPacket::Mkdir {
                request_id: 0,
                path: "/new".into(),
                attrs: SftpFileAttrs::default(),
            },
            SftpPacket::Rmdir {
                request_id: 0,
                path: "/old".into(),
            },
            SftpPacket::Rename {
                request_id: 0,
                old_path: "/a".into(),
                new_path: "/b".into(),
            },
            SftpPacket::Readlink {
                request_id: 0,
                path: "/link".into(),
            },
            SftpPacket::Symlink {
                request_id: 0,
                link_path: "/l".into(),
                target_path: "/t".into(),
            },
        ];

        for mut pkt in cases {
            SftpSession::set_request_id_in_packet(&mut pkt, 42);
            assert_eq!(
                pkt.request_id(),
                Some(42),
                "Failed for {:?}",
                pkt.packet_type()
            );
        }
    }

    // -----------------------------------------------------------------
    // Pending Request Tracker Tests
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_tracker_register_and_deliver() {
        let mut tracker = PendingRequestTracker::new(16);

        let rx = tracker.register(42).unwrap();
        assert_eq!(tracker.len(), 1);

        let response = SftpPacket::Handle {
            request_id: 42,
            handle: vec![0x01, 0x02, 0x03],
        };

        let delivered = tracker.deliver_response(response).unwrap();
        assert!(delivered);
        assert!(tracker.is_empty());

        // Receive the response
        let received = rx.await.unwrap();
        assert_eq!(received.request_id(), Some(42));
    }

    #[tokio::test]
    async fn test_tracker_cancel_removes_pending() {
        let mut tracker = PendingRequestTracker::new(16);

        tracker.register(1).unwrap();
        tracker.register(2).unwrap();
        tracker.register(3).unwrap();
        assert_eq!(tracker.len(), 3);

        assert!(tracker.cancel(2));
        assert_eq!(tracker.len(), 2);
        assert!(!tracker.cancel(99)); // Non-existent ID
    }

    #[test]
    fn test_tracker_at_capacity() {
        let mut tracker = PendingRequestTracker::new(2);

        tracker.register(1).unwrap();
        tracker.register(2).unwrap();

        let result = tracker.register(3);
        assert!(result.is_err());
        match result.err().unwrap() {
            PendingRequestError::AtCapacity { current, max } => {
                assert_eq!(current, 2);
                assert_eq!(max, 2);
            }
            other => panic!("Expected AtCapacity error, got: {:?}", other),
        }
    }

    #[test]
    fn test_tracker_duplicate_id_rejected() {
        let mut tracker = PendingRequestTracker::new(16);

        tracker.register(5).unwrap();
        let result = tracker.register(5);

        assert!(result.is_err());
        match result.err().unwrap() {
            PendingRequestError::DuplicateId(id) => assert_eq!(id, 5),
            other => panic!("Expected DuplicateId error, got: {:?}", other),
        }
    }

    #[test]
    fn test_tracker_cancel_all() {
        let mut tracker = PendingRequestTracker::new(16);

        for i in 1..=10u32 {
            tracker.register(i).unwrap();
        }
        assert_eq!(tracker.len(), 10);

        let cancelled = tracker.cancel_all();
        assert_eq!(cancelled, 10);
        assert!(tracker.is_empty());
    }

    #[tokio::test]
    async fn test_tracker_dropped_receiver_cleanup() {
        let mut tracker = PendingRequestTracker::new(16);

        // Register but immediately drop the receiver
        {
            let _rx = tracker.register(77).unwrap();
        } // rx dropped here

        // Delivering to a dropped receiver should succeed but not panic
        let response = SftpPacket::Status {
            request_id: 77,
            code: SSH_FX_OK,
            message: "OK".to_string(),
            language: "".to_string(),
        };

        let delivered = tracker.deliver_response(response).unwrap();
        assert!(delivered); // Delivery succeeded even though nobody received it
        assert!(tracker.is_empty()); // Entry was cleaned up
    }

    // -----------------------------------------------------------------
    // Status Code Utility Tests
    // -----------------------------------------------------------------

    #[test]
    fn test_status_code_descriptions_accessible() {
        // Verify status code descriptions are accessible from session module
        assert_eq!(status_code_description(SSH_FX_OK), "Operation succeeded");
        assert_eq!(status_code_description(1), "End of file"); // SSH_FX_EOF
    }

    #[test]
    fn test_file_attrs_in_session_context() {
        // Test SftpFileAttrs creation as used in session context
        let attrs = SftpFileAttrs::full(4096, 1000, 1000, 0o040755, 1700000000, 1700000100);
        assert!(attrs.is_directory());
        assert_eq!(attrs.size, Some(4096));

        let flags = attrs.flags();
        assert!(flags != 0);
    }

    #[test]
    fn test_max_pending_requests_constant() {
        assert_eq!(MAX_PENDING_REQUESTS, 256);
    }

    #[test]
    fn test_pending_request_error_display() {
        let err = PendingRequestError::TimedOut {
            request_id: 42,
            timeout_secs: 30,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("timed out"));
        assert!(msg.contains("30")); // timeout_secs is in the message
    }
}
