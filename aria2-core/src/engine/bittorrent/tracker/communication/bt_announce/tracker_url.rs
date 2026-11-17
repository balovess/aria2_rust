//! Public helpers retained by the BitTorrent announce state machine.

/// Returns whether `url` uses the UDP tracker transport.
///
/// The protocol crate owns URL scheme parsing; this function remains as a
/// compatibility entry point for users of the public core module.
pub fn is_udp_tracker(url: &str) -> bool {
    aria2_protocol::bittorrent::tracker::url::is_udp_tracker_url(url)
}

pub use aria2_protocol::bittorrent::tracker::http_tracker_protocol::urlencode_infohash;
