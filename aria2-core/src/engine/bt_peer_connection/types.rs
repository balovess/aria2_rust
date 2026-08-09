//! Small shared types for the BitTorrent peer connection module.
//!
//! Contains [`ConnectionType`] and [`SendBuffer`], which are used across
//! multiple sub-modules.

// ===========================================================================
// ConnectionType
// ===========================================================================

/// Type of peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    /// Standard TCP connection.
    Tcp,
    /// uTP (UDP-based) connection.
    Utp,
}

// ===========================================================================
// SendBuffer — outbound message buffer (C++ SocketBuffer)
// ===========================================================================

/// Outbound message buffer for batching small messages into larger TCP writes.
///
/// Mirrors the C++ `SocketBuffer`: messages are pushed into the buffer and
/// only written to the socket when flushed. This reduces the number of
/// syscalls and improves throughput, especially when sending multiple small
/// messages (e.g., a burst of Have messages).
pub struct SendBuffer {
    /// Queued message bytes, waiting to be written to the socket.
    pending: Vec<u8>,
    /// Whether encryption is enabled for this buffer.
    encryption_enabled: bool,
}

impl SendBuffer {
    /// Create a new empty send buffer.
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            encryption_enabled: false,
        }
    }

    /// Add data to the pending buffer.
    ///
    /// In a future iteration, when `encryption_enabled` is `true`, the data
    /// will be encrypted before being queued. For now the flag is stored but
    /// does not affect the data.
    pub fn push_bytes(&mut self, data: Vec<u8>) {
        // TODO: encrypt data if encryption_enabled
        self.pending.extend_from_slice(&data);
    }

    /// Check whether the pending buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Get the number of bytes in the pending buffer.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Clear the pending buffer.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Drain the pending data, returning it as a `Vec<u8>` for writing to
    /// the socket.
    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// Set whether encryption is enabled for this buffer.
    pub fn set_encryption_enabled(&mut self, enabled: bool) {
        self.encryption_enabled = enabled;
    }

    /// Check whether encryption is enabled.
    pub fn is_encryption_enabled(&self) -> bool {
        self.encryption_enabled
    }
}

impl Default for SendBuffer {
    fn default() -> Self {
        Self::new()
    }
}
