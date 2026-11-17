//! Compatibility re-export for the public core path of peer session state.
//!
//! The implementation belongs to `aria2_protocol::bittorrent::peer` because
//! it contains BitTorrent peer availability and negotiated protocol state.
pub use aria2_protocol::bittorrent::peer::session_resource::PeerSessionResource;
