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

pub mod message_handler;
pub mod peer_message_handler;
#[cfg(test)]
mod tests;
pub mod types;

// Public re-exports — all public items remain accessible from `bt_message_handler::`
pub use message_handler::BtMessageHandler;
pub use peer_message_handler::BtPeerMessageHandler;
pub use types::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, BlockDownloadResult, DEFAULT_MAX_OUTSTANDING_REQUEST,
    MAX_BLOCK_READ_MESSAGES, MAX_RETRIES, PeerStateUpdate, RequestResponse,
    UB_MAX_OUTSTANDING_REQUEST,
};
