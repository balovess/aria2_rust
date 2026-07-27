//! BitTorrent tracker communication module.
//!
//! Provides tracker announce lifecycle management, multi-tier tracker lists,
//! and HTTP/HTTPS/UDP tracker announce functionality.
//!
//! # Architecture
//!
//! - [`BtAnnounce`] — core announce state machine (timing, events, tier rotation)
//! - [`TrackerAnnouncer`] — unified dispatcher routing HTTP/UDP through BtAnnounce
//! - [`AnnounceList`] — multi-tier tracker URL management with failover
//! - [`AnnounceResult`] — unified result type for HTTP and UDP announce responses

mod announce_list;
mod bt_announce;
mod health_tracking;
mod tracker_announce;
mod types;

#[cfg(test)]
mod tests;

// Public re-exports — preserve the original `bt_tracker_comm::*` API surface.
pub use announce_list::{AnnounceList, AnnounceTier};
pub use bt_announce::{
    BtAnnounce, announce_to_public_tracker, announce_to_public_tracker_with_event,
    is_udp_tracker, perform_announce_with_event, perform_http_tracker_announce, urlencode_infohash,
};
pub use health_tracking::HealthTrackingAnnounceList;
pub use tracker_announce::{AnnounceResult, TrackerAnnouncer};
pub use types::{AnnounceEvent, TrackerEntry, TrackerTier};
