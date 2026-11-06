//! BitTorrent task orchestration and runtime state.
//!
//! Peer transport and wire formats live in `aria2-protocol`; this module groups
//! torrent downloads, peer/piece policy, trackers, discovery, and persistence.

pub mod peer;

#[cfg(feature = "bittorrent")]
pub(crate) mod command_adapter;
#[cfg(feature = "bittorrent")]
pub mod dht;
#[cfg(feature = "bittorrent")]
pub mod discovery;
#[cfg(feature = "bittorrent")]
pub mod download;
#[cfg(feature = "bittorrent")]
pub mod magnet;
#[cfg(feature = "bittorrent")]
pub mod persistence;
#[cfg(feature = "bittorrent")]
pub mod piece;
#[cfg(feature = "bittorrent")]
pub mod registry;
#[cfg(feature = "bittorrent")]
pub mod torrent;
#[cfg(feature = "bittorrent")]
pub mod tracker;

#[cfg(all(test, feature = "bittorrent"))]
mod tests;
