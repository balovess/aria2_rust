//! BitTorrent peer connection abstraction with send buffering, session resources,
//! and keep-alive management.
//!
//! Mirrors the C++ aria2 architecture of `Peer` + `PeerSessionResource` +
//! `PeerConnection` + `SocketBuffer`:
//!
//! - [`SendBuffer`] — outbound message buffer that batches small messages
//!   into larger TCP writes (C++ `SocketBuffer`).
//! - [`PeerSessionResource`] — per-session state allocated when a peer becomes
//!   active and released on disconnect (C++ `PeerSessionResource`).
//! - [`BtPeerConn`] — the public connection type that composes the above with
//!   keep-alive management, bitfield delegation, and the existing inner
//!   connection variants.
//!
//! # Keep-alive
//!
//! Per the BitTorrent spec, peers must send a keep-alive message every
//! ~2 minutes if no other message has been sent. The connection is
//! considered dead after ~3 minutes of inactivity.

mod peer_conn;
mod session_resource;
#[cfg(test)]
mod tests;
mod types;
mod utp_connection;

// Public re-exports — preserve the original API surface.
pub use peer_conn::BtPeerConn;
pub use session_resource::PeerSessionResource;
pub use types::{ConnectionType, SendBuffer};
pub use utp_connection::UtpPeerConnection;

// crate-visible re-export: InnerConnection was pub(crate) in the original file.
pub(crate) use peer_conn::InnerConnection;
