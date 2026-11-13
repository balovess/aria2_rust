//! UDP tracker integration for the announce state machine.

use std::time::Duration;
use tracing::debug;

use aria2_protocol::bittorrent::tracker::url::is_udp_tracker_url;

use super::{AnnounceEvent, BtAnnounce};

impl BtAnnounce {
    // ==================================================================
    // UDP Tracker Integration
    // ==================================================================

    /// Returns `true` if the current tracker URL is a UDP tracker.
    ///
    /// Call this after `adjust_announce_list()` to determine whether
    /// to route the announce through the UDP client or HTTP path.
    ///
    /// # C++ Reference
    ///
    /// C++ aria2 dispatches to `UdpTrackerRequest` based on the URL scheme
    /// during `BtAnnounce::announce()`.
    pub fn is_current_tracker_udp(&self) -> bool {
        self.announce_list
            .get_announce()
            .map(is_udp_tracker_url)
            .unwrap_or(false)
    }

    /// Convert the current announce event to a UDP tracker event.
    ///
    /// Maps the tracker state machine event to the appropriate UDP protocol
    /// event for use with `UdpTrackerClient::announce()`.
    ///
    /// # C++ Reference
    ///
    /// C++ `UdpTrackerRequest` uses the same event values:
    /// - 0 = none, 1 = completed, 2 = started, 3 = stopped
    pub fn current_udp_event(
        &self,
    ) -> aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent {
        use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent;
        match self.announce_list.get_event() {
            AnnounceEvent::Started | AnnounceEvent::StartedAfterCompletion => UdpEvent::Started,
            AnnounceEvent::Completed => UdpEvent::Completed,
            AnnounceEvent::Stopped => UdpEvent::Stopped,
            // Downloading, Seeding, Halted are periodic announces with no event
            AnnounceEvent::Downloading | AnnounceEvent::Seeding | AnnounceEvent::Halted => {
                UdpEvent::None
            }
        }
    }

    /// Process a UDP tracker announce response and update internal state.
    ///
    /// This is the UDP equivalent of `process_announce_response()`. It
    /// updates the interval, seeder/leecher counts, and announces success.
    ///
    /// Returns peer addresses on success.
    pub fn process_udp_announce_response(
        &mut self,
        response: &aria2_protocol::bittorrent::tracker::udp_tracker_protocol::AnnounceResponse,
    ) -> Vec<(String, u16)> {
        // Update interval from response
        // C++ sets both minInterval_ and interval_ to the reply interval directly
        if response.interval > 0 {
            let new_interval = Duration::from_secs(response.interval as u64);
            self.interval = new_interval;
            self.min_interval = new_interval;
            debug!("[BT] UDP tracker interval: {}s", response.interval);
        }

        // Update complete/incomplete counts
        self.complete = Some(response.seeders as i64);
        self.incomplete = Some(response.leechers as i64);
        debug!(
            "[BT] UDP tracker stats: seeders={}, leechers={}",
            response.seeders, response.leechers
        );

        // Mark announce success (resets in-flight counter, updates timing)
        self.announce_success();

        response.peers.clone()
    }
}
