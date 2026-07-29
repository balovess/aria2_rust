use std::net::SocketAddr;
use std::time::Instant;

use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::{
    AnnounceResponse, ScrapeResult, UdpError, UdpEvent, UdpState,
};

/// Internal request representation for UDP tracker operations.
pub(crate) struct UdpTrackerRequest {
    pub(crate) remote_addr: SocketAddr,
    pub(crate) info_hash: [u8; 20],
    pub(crate) peer_id: [u8; 20],
    pub(crate) downloaded: i64,
    pub(crate) left: i64,
    pub(crate) uploaded: i64,
    pub(crate) event: UdpEvent,
    pub(crate) num_want: i32,
    pub(crate) port: u16,
    pub(crate) state: UdpState,
    pub(crate) error: Option<UdpError>,
    pub(crate) dispatched_at: Option<Instant>,
    pub(crate) fail_count: u32,
    pub(crate) reply: Option<AnnounceResponse>,
    /// Scrape results populated when this is a scrape request
    pub(crate) scrape_results: Option<Vec<ScrapeResult>>,
    /// Info hashes for scrape requests (can be multiple)
    pub(crate) scrape_info_hashes: Vec<[u8; 20]>,
    pub(crate) txn_id: u32,
}

impl UdpTrackerRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        addr: SocketAddr,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
        downloaded: i64,
        left: i64,
        uploaded: i64,
        event: UdpEvent,
        num_want: i32,
        port: u16,
    ) -> Self {
        Self {
            remote_addr: addr,
            info_hash,
            peer_id,
            downloaded,
            left,
            uploaded,
            event,
            num_want,
            port,
            state: UdpState::Pending,
            error: None,
            dispatched_at: None,
            fail_count: 0,
            reply: None,
            scrape_results: None,
            scrape_info_hashes: Vec::new(),
            txn_id: 0,
        }
    }
}
