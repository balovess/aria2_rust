//! BtMessageHandler — legacy stateless block request/receive utilities.
//!
//! Manages the process of requesting and receiving individual blocks
//! from peers during piece download.
//!
//! # Deprecation Note
//!
//! This struct provides only static methods with no per-peer state.
//! For new code, prefer [`super::BtPeerMessageHandler`] which integrates with
//! [`BtMessageDispatcher`](crate::engine::bt_message_dispatcher::BtMessageDispatcher)
//! for request slot tracking, event-driven actions, flooding detection,
//! and timeout checking.

mod endgame;
mod normal;
mod pipelined;

/// BT Message Handler for block-level operations (legacy, stateless).
///
/// Manages the process of requesting and receiving individual blocks
/// from peers during piece download.
///
/// # Deprecation Note
///
/// This struct provides only static methods with no per-peer state.
/// For new code, prefer [`super::BtPeerMessageHandler`] which integrates with
/// [`BtMessageDispatcher`](crate::engine::bt_message_dispatcher::BtMessageDispatcher)
/// for request slot tracking, event-driven actions, flooding detection,
/// and timeout checking.
#[allow(dead_code)]
pub struct BtMessageHandler;
