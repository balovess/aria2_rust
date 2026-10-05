//! BitTorrent tracker communication module.
//!
//! Provides tracker announce lifecycle management, multi-tier tracker lists,
//! and HTTP/HTTPS/WebSocket/UDP tracker announce functionality.
//!
//! # Architecture
//!
//! - [`BtAnnounce`] — core announce state machine (timing, events, tier rotation)
//! - [`TrackerAnnouncer`] — unified dispatcher routing HTTP/WebSocket/UDP through BtAnnounce
//! - [`AnnounceList`] — multi-tier tracker URL management with failover
//! - [`AnnounceResult`] — unified result type for HTTP, WebSocket, and UDP announce responses

mod announce_list;
mod bt_announce;
mod tracker_announce;
mod types;

#[cfg(test)]
mod tests;

pub use announce_list::{AnnounceList, AnnounceTier};
pub use aria2_protocol::bittorrent::tracker::public_list::TrackerFailureKind;
pub use bt_announce::{BtAnnounce, is_udp_tracker, urlencode_infohash};
pub use tracker_announce::{
    AnnounceResult, SharedTrackerRuntime, TrackerAnnouncer, TrackerRuntimeInfo,
    TrackerRuntimeSnapshot,
};
pub use types::AnnounceEvent;
