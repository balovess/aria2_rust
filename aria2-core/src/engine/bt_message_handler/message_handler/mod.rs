//! BtMessageHandler — stateless block request/receive utilities.
//!
//! Manages the process of requesting and receiving individual blocks
//! from peers during piece download.
//!
//! This is the block-download path. It owns the per-piece request window and
//! coordinates transfers that may span multiple peers.

mod endgame;
mod endgame_pipeline;
mod normal;
mod normal_pipeline;
mod peer_worker;
mod pipelined;

pub(crate) use peer_worker::{PeerActorTask, PeerCommand, PeerEvent};

/// BT message handler for block-level operations.
///
/// Manages the process of requesting and receiving individual blocks
/// from peers during piece download.
///
/// This type intentionally has no per-peer state. It owns the block-download
/// operations shared by normal, pipelined, and endgame transfers.
pub struct BtMessageHandler;
