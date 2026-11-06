//! Small shared types for the BitTorrent peer connection module.
//!
//! Contains [`ConnectionType`] and the internal send buffer used across
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
pub(crate) struct SendBuffer {
    /// Queued message bytes, waiting to be written to the socket.
    pending: Vec<u8>,
}

impl SendBuffer {
    /// Create a new empty send buffer.
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Add data to the pending buffer.
    pub(crate) fn push_bytes(&mut self, data: Vec<u8>) {
        self.pending.extend_from_slice(&data);
    }

    /// Check whether the pending buffer is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Drain the pending data, returning it as a `Vec<u8>` for writing to
    /// the socket.
    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

impl Default for SendBuffer {
    fn default() -> Self {
        Self::new()
    }
}
