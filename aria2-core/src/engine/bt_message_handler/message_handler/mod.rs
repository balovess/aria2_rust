//! BtMessageHandler — stateless block request/receive utilities.
//!
//! Manages the process of requesting and receiving individual blocks
//! from peers during piece download.
//!
//! This is the block-download path. It owns the per-piece request window and
//! coordinates transfers that may span multiple peers.

mod endgame;
mod normal;
mod pipelined;

/// BT message handler for block-level operations.
///
/// Manages the process of requesting and receiving individual blocks
/// from peers during piece download.
///
/// This type intentionally has no per-peer state. It owns the block-download
/// operations shared by normal, pipelined, and endgame transfers.
pub struct BtMessageHandler;
