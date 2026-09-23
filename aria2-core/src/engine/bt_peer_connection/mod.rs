//! BitTorrent peer connection abstraction with send buffering, session resources,
//! and keep-alive management.
//!
//! Mirrors the C++ aria2 architecture of `Peer` + `PeerSessionResource` +
//! `PeerConnection` + `SocketBuffer`:
//!
//! - An internal send buffer that batches small messages into larger TCP
//!   writes (C++ `SocketBuffer`).
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
pub(crate) use peer_conn::PeerActorId;
pub use peer_conn::{MseConnectionOptions, UtpConnectionOptions};
mod session_resource;
#[cfg(test)]
mod tests;
mod types;
mod utp_connection;
#[cfg(test)]
mod utp_connection_tests;

// Public connection types and their supporting data structures.
pub use peer_conn::BtPeerConn;
pub use session_resource::PeerSessionResource;
pub use types::ConnectionType;
pub use utp_connection::UtpPeerConnection;
