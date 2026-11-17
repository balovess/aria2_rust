//! Core-owned BitTorrent peer connection state and transport adapter.
//!
//! [`BtPeerConn`] composes the protocol transport with engine-owned peer
//! identity, statistics, upload policy state, and session lifecycle. The
//! `PeerActor` owns it while the connection is active and is the only runtime
//! reader/writer. Wire parsing, handshake, and message validation are provided
//! by `aria2-protocol`.
//!
//! # Keep-alive
//!
//! Per the BitTorrent spec, peers must send a keep-alive message every
//! ~2 minutes if no other message has been sent. The connection is
//! considered dead after ~3 minutes of inactivity.

mod peer_conn;
pub(crate) use peer_conn::PeerActorId;
#[cfg(test)]
pub(crate) use peer_conn::PeerActorStartup;
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
