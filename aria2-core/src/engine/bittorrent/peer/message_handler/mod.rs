//! BT Message Handler - Block request and receive logic
//!
//! This module handles the low-level BitTorrent protocol message processing
//! for block requests and data reception during piece download.
//!
//! Extracted from `bt_download_command.rs` to improve modularity and
//! follow the single responsibility principle.
//!
#[path = "peer_scheduler/mod.rs"]
mod peer_scheduler;
pub mod types;

pub(crate) use peer_scheduler::{
    PeerActorPayloadConfig, PeerCommand, PeerEvent, PeerSwarm, PeerSwarmEventLease,
    download_piece_blocks, download_piece_blocks_batch, download_piece_blocks_endgame,
};
pub use types::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, DEFAULT_MAX_OUTSTANDING_REQUEST, MAX_RETRIES,
    PieceDownloadResult,
};
