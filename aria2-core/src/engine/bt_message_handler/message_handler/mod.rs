//! BtMessageHandler — stateless block request/receive utilities.
//!
//! Manages the process of requesting and receiving individual blocks
//! from peers during piece download.
//!
//! This is the block-download path. Per-peer control-message state is owned by
//! [`super::BtPeerMessageHandler`], while this handler coordinates block
//! transfers that may span multiple peers.

mod endgame;
mod normal;
mod pipelined;

/// BT message handler for block-level operations.
///
/// Manages the process of requesting and receiving individual blocks
/// from peers during piece download.
///
/// This type intentionally has no per-peer state. It owns the block-download
/// operations that are shared by normal, pipelined, and endgame transfers;
/// per-peer protocol state remains in [`super::BtPeerMessageHandler`].
pub struct BtMessageHandler;
