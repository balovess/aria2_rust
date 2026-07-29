//! URL detection and encoding helpers for BitTorrent tracker URLs.
//!
//! Provides [`is_udp_tracker`] for protocol detection, and URL-encoding
//! functions [`urlencode_infohash`] and [`urlencode_bytes`] required by
//! the BitTorrent tracker announce URL specification.

// ======================================================================
// URL Detection Helpers
// ======================================================================

/// Returns `true` if the URL uses the UDP tracker protocol (`udp://`).
///
/// This is used by the BtAnnounce integration to route UDP tracker URLs
/// to the `UdpTrackerManager` instead of the HTTP announce path.
///
/// # C++ Reference
///
/// C++ aria2 routes UDP tracker announces through `DHTSetup` which creates
/// `UdpTrackerRequest` objects for `udp://` URLs, while HTTP trackers
/// use `HttpRequestCommand` → `HttpResponseCommand`.
pub fn is_udp_tracker(url: &str) -> bool {
    url.to_lowercase().starts_with("udp://")
}

// ======================================================================
// URL Encoding Helpers
// ======================================================================

/// URL-encodes a 20-byte info hash or peer ID for use in tracker URLs.
///
/// Each byte is encoded as `%XX` where XX is the uppercase hex representation.
/// This is required by the BitTorrent tracker protocol specification.
pub fn urlencode_infohash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("%{:02X}", b)).collect()
}

/// URL-encodes an arbitrary byte slice for use in tracker URLs.
pub(crate) fn urlencode_bytes(data: &[u8]) -> String {
    data.iter().map(|b| format!("%{:02X}", b)).collect()
}
