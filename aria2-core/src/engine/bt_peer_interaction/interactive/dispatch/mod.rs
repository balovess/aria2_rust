//! Message dispatch, receive, flooding detection, and outstanding-request
//! scaling for `BtPeerInteractive`. Also includes handshake reception and
//! same-peer-ID duplicate detection integration.
//!
//! Sub-modules:
//! - [interaction] - Main interaction processing loop (`do_interaction_processing`)
//! - [dispatch_message] - Central message dispatch (`dispatch_message`)
//! - [receive] - Message reception loop (`receive_messages`)
//! - [handshake] - Handshake peer-ID validation (`validate_handshake_peer_id`)

pub mod interaction;
pub mod dispatch_message;
pub mod receive;
pub mod handshake;
