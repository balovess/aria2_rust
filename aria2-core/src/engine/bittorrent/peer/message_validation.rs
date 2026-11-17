//! Compatibility re-exports for the public core paths of protocol validation.
//!
//! The implementation is owned by `aria2_protocol::bittorrent::peer` because
//! it validates BitTorrent messages and torrent geometry without engine policy.
pub use aria2_protocol::bittorrent::peer::message_validation::{
    BtMessageValidationError, BtMessageValidator, MAX_BLOCK_LENGTH,
};
