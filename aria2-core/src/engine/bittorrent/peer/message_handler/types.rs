//! Types, constants, and enums for BT message handling.

use crate::constants;
use crate::engine::bittorrent::peer::connection::PeerActorId;
use std::net::SocketAddr;

/// Block size for each piece block request (16 KB)
pub const BLOCK_SIZE: u32 = constants::BT_BLOCK_SIZE as u32;

/// Maximum number of retries for a failed piece download
pub const MAX_RETRIES: u32 = constants::BT_MAX_RETRIES;

/// Timeout for each block request (seconds)
pub const BLOCK_REQUEST_TIMEOUT_SECS: u64 = constants::BT_BLOCK_REQUEST_TIMEOUT_SECS;

/// Default maximum outstanding requests per peer.
/// Matches C++ `DEFAULT_MAX_OUTSTANDING_REQUEST = 6` (BtConstants.h).
pub const DEFAULT_MAX_OUTSTANDING_REQUEST: usize = constants::BT_DEFAULT_MAX_OUTSTANDING_REQUEST;

/// Maximum adaptive outstanding requests per peer.
/// Matches C++ `UB_MAX_OUTSTANDING_REQUEST = 256` (BtConstants.h).
pub const MAX_OUTSTANDING_REQUEST: usize = 256;

/// Bytes supplied by a peer during a piece download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerDownloadBytes {
    pub peer_index: usize,
    pub peer: SocketAddr,
    pub bytes: u64,
}

/// Data for a complete piece and the peers that supplied its blocks.
#[derive(Debug, PartialEq, Eq)]
pub struct PieceDownloadResult {
    pub data: Vec<u8>,
    /// Per-peer bytes supplied in this piece attempt.
    pub peer_bytes: Vec<PeerDownloadBytes>,
    /// Concrete peers that failed while this piece was being downloaded.
    pub failed_peers: Vec<SocketAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PieceRequestPlan {
    pub(crate) piece_index: u32,
    pub(crate) piece_length: u32,
    pub(crate) num_blocks: u32,
}

/// Internal result carrying stable peer identities alongside the public
/// endpoint/index attribution contract.
pub(crate) struct ActorAwarePieceDownloadResult {
    pub(crate) piece: PieceDownloadResult,
    /// Actor IDs aligned with `piece.peer_bytes`.
    pub(crate) peer_actor_ids: Vec<PeerActorId>,
    /// Peers whose availability changed while this piece was in flight.
    pub(crate) availability_changed_actor_ids: Vec<PeerActorId>,
    /// Peers discovered through negotiated BEP 11 messages during the attempt.
    pub(crate) pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    /// Peers discovered by the torrent-scoped tracker actor during the attempt.
    pub(crate) tracker_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
}

pub(crate) struct ActorAwarePieceBatchEntry {
    pub(crate) piece_index: u32,
    pub(crate) result: std::result::Result<PieceDownloadResult, String>,
    /// Actor IDs aligned with `result.peer_bytes` when the piece completed.
    pub(crate) peer_actor_ids: Vec<PeerActorId>,
}

pub(crate) struct ActorAwarePieceBatchResult {
    pub(crate) pieces: Vec<ActorAwarePieceBatchEntry>,
    pub(crate) availability_changed_actor_ids: Vec<PeerActorId>,
    pub(crate) pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    pub(crate) tracker_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
}
