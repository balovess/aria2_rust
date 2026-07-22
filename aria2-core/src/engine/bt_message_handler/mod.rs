//! BT Message Handler - Block request and receive logic
//!
//! This module handles the low-level BitTorrent protocol message processing
//! for block requests and data reception during piece download.
//!
//! Extracted from `bt_download_command.rs` to improve modularity and
//! follow the single responsibility principle.
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/DefaultBtMessageDispatcher.h` - Message queue + request slots
//! - `src/DefaultBtInteractive.h` - Per-peer interaction loop
//! - `src/PeerInteractionCommand.h` - Peer interaction
//!
//! # Module Structure
//!
//! - [`BtPeerMessageHandler`] — Per-peer stateful handler wrapping a
//!   [`BtMessageDispatcher`](crate::engine::bt_message_dispatcher::BtMessageDispatcher)
//!   with event-driven actions, flooding detection, and request slot tracking.
//!   Mirrors C++ `DefaultBtInteractive`.
//! - [`BtMessageHandler`] — Legacy stateless block request/receive utilities
//!   (kept for backward compatibility; prefer `BtPeerMessageHandler`).

pub mod types;
pub mod peer_message_handler;
pub mod message_handler;
#[cfg(test)]
mod tests;

// Public re-exports — all public items remain accessible from `bt_message_handler::`
pub use types::{
    BLOCK_SIZE, MAX_RETRIES, BLOCK_REQUEST_TIMEOUT_SECS, MAX_BLOCK_READ_MESSAGES,
    DEFAULT_MAX_OUTSTANDING_REQUEST, UB_MAX_OUTSTANDING_REQUEST,
    PeerStateUpdate, RequestResponse, BlockDownloadResult,
};
pub use peer_message_handler::BtPeerMessageHandler;
pub use message_handler::BtMessageHandler;
