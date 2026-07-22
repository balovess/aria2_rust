//! BitTorrent tracker communication module.
//!
//! Provides tracker announce lifecycle management, multi-tier tracker lists,
//! and HTTP/HTTPS tracker announce functionality.

mod announce_list;
mod bt_announce;
mod health_tracking;
mod types;

#[cfg(test)]
mod tests;

// Public re-exports — preserve the original `bt_tracker_comm::*` API surface.
pub use announce_list::{AnnounceList, AnnounceTier};
pub use bt_announce::{
    announce_to_public_tracker, announce_to_public_tracker_with_event, perform_announce_with_event,
    perform_http_tracker_announce, urlencode_infohash, BtAnnounce,
};
pub use health_tracking::HealthTrackingAnnounceList;
pub use types::{AnnounceEvent, TrackerEntry, TrackerTier};
