//! BitTorrent tracker communication module.
//!
//! Provides tracker announce lifecycle management, multi-tier tracker lists,
//! and HTTP/HTTPS/WebSocket/UDP tracker announce functionality.
//!
//! # Architecture
//!
//! - [`BtAnnounce`] — core announce timing and task lifecycle coordination
//! - [`TrackerAnnouncer`] — unified dispatcher routing HTTP/WebSocket/UDP through BtAnnounce
//! - [`AnnounceList`] — protocol-owned multi-tier tracker order and failover state
//! - [`AnnounceResult`] — unified result type for HTTP, WebSocket, and UDP announce responses

mod bt_announce;
mod tracker_announce;

#[cfg(test)]
mod tests;

pub use aria2_protocol::bittorrent::tracker::announce_list::{
    AnnounceEvent, AnnounceList, AnnounceTier,
};
pub use aria2_protocol::bittorrent::tracker::public_list::TrackerFailureKind;
pub use bt_announce::{BtAnnounce, is_udp_tracker, urlencode_infohash};
pub use tracker_announce::{
    AnnounceResult, SharedTrackerRuntime, TrackerAnnouncer, TrackerRuntimeInfo,
    TrackerRuntimeSnapshot,
};
