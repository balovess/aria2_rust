//! URL detection and encoding helpers for BitTorrent tracker URLs.
//!
//! Provides [`is_udp_tracker`] for engine routing and re-exports the public
//! [`urlencode_infohash`] helper from the protocol crate.

// ======================================================================
// URL Detection Helpers
// ======================================================================

/// Returns `true` if the URL uses the UDP tracker protocol (`udp://`).
///
/// This is used by `TrackerAnnouncer` to route UDP URLs to the UDP client.
///
/// # C++ Reference
///
/// C++ aria2 routes UDP tracker announces through `DHTSetup` which creates
/// `UdpTrackerRequest` objects for `udp://` URLs, while HTTP trackers
/// use `HttpRequestCommand` → `HttpResponseCommand`.
pub fn is_udp_tracker(url: &str) -> bool {
    url.get(..6)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("udp://"))
}

// ======================================================================
// URL Encoding Helpers
// ======================================================================

pub use aria2_protocol::bittorrent::tracker::http_tracker_protocol::urlencode_infohash;
