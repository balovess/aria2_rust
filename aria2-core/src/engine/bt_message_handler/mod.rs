//! BT Message Handler - Block request and receive logic
//!
//! This module handles the low-level BitTorrent protocol message processing
//! for block requests and data reception during piece download.
//!
//! Extracted from `bt_download_command.rs` to improve modularity and
//! follow the single responsibility principle.
//!
//! The active download path is implemented by [`BtMessageHandler`], which
//! owns the per-piece request window and peer-worker lifecycle.

pub mod message_handler;
pub mod types;

pub use message_handler::BtMessageHandler;
pub(crate) use message_handler::{PeerActorTask, PeerCommand, PeerEvent};
pub use types::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, DEFAULT_MAX_OUTSTANDING_REQUEST, MAX_RETRIES,
    PieceDownloadResult,
};
