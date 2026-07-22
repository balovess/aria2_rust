//! Types, constants, and enums for BT message handling.

use crate::constants;

/// Block size for each piece block request (16 KB)
pub const BLOCK_SIZE: u32 = constants::BT_BLOCK_SIZE as u32;

/// Maximum number of retries for a failed piece download
pub const MAX_RETRIES: u32 = constants::BT_MAX_RETRIES;

/// Timeout for each block request (seconds)
pub const BLOCK_REQUEST_TIMEOUT_SECS: u64 = constants::BT_BLOCK_REQUEST_TIMEOUT_SECS;

/// Maximum messages to read while waiting for a specific block
pub const MAX_BLOCK_READ_MESSAGES: u32 = constants::BT_MAX_BLOCK_READ_MESSAGES as u32;

/// Default maximum outstanding requests per peer.
/// Matches C++ `DEFAULT_MAX_OUTSTANDING_REQUEST = 6` (BtConstants.h).
pub const DEFAULT_MAX_OUTSTANDING_REQUEST: usize =
    constants::BT_DEFAULT_MAX_OUTSTANDING_REQUEST;

/// Upper bound for max outstanding request auto-scaling.
/// Matches C++ `UB_MAX_OUTSTANDING_REQUEST = 256` (BtConstants.h).
pub const UB_MAX_OUTSTANDING_REQUEST: usize = constants::BT_UB_MAX_OUTSTANDING_REQUEST;

// ======================================================================
// PeerStateUpdate — side-effect update for the caller
// ======================================================================

/// Side-effect update that the caller must apply to the peer and piece storage.
///
/// The handler does not own `PieceStorage` or `PeerStorage`, so it returns
/// these updates for the caller to apply. This mirrors the C++ pattern where
/// `doReceivedAction()` mutates peer/piece-storage directly.
#[derive(Debug, Clone)]
pub enum PeerStateUpdate {
    /// The peer now has the given piece index (Have message).
    HavePiece { index: u32 },
    /// The peer's bitfield has been set to the given data (Bitfield message).
    SetBitfield { data: Vec<u8> },
    /// The peer is now a seeder — has all pieces (HaveAll message).
    MarkSeeder,
    /// The peer has no pieces (HaveNone message).
    ClearBitfield,
    /// The choking algorithm should be re-evaluated (Interested/NotInterested
    /// when choking state is relevant).
    ExecuteChoke,
    /// Disconnect: the peer is a seeder and our download is finished.
    DisconnectSeeder,
}

// ======================================================================
// RequestResponse — response to an incoming Request message
// ======================================================================

/// Response to an incoming Request message (ID=6).
///
/// Mirrors C++ `BtRequestMessage::doReceivedAction()` which either queues a
/// Piece message, a Reject message, or drops the request silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestResponse {
    /// Queue a Piece message with the given data.
    Piece {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// Queue a Reject message (fast extension).
    Reject {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// Drop the request silently (choking without fast extension).
    None,
}

// ======================================================================
// BlockDownloadResult — result of a block download attempt
// ======================================================================

/// Result of a block download attempt
pub struct BlockDownloadResult {
    /// Whether the block was successfully received
    pub success: bool,
    /// The received data (if successful)
    pub data: Option<Vec<u8>>,
    /// Number of bytes received (for statistics)
    pub bytes_received: u64,
}
