//! Tracker announce URL scheme classification.

/// The transport selected for a BitTorrent tracker announce URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerTransport {
    Http,
    Https,
    Udp,
    WebSocket,
}

/// Returns whether the raw URL begins with the UDP tracker scheme.
///
/// This intentionally recognizes the scheme before validating the rest of the
/// URL so malformed UDP URLs still reach the UDP client's URL error path.
pub fn is_udp_tracker_url(url: &str) -> bool {
    url.get(..6)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("udp://"))
}

/// Parse a tracker URL and return its supported announce transport.
///
/// This classifies the URL scheme only. Callers still own transport setup,
/// address resolution, proxy selection, and outbound network policy.
pub fn classify_tracker_url(url: &str) -> Option<TrackerTransport> {
    let url = url::Url::parse(url).ok()?;
    match url.scheme() {
        "http" => Some(TrackerTransport::Http),
        "https" => Some(TrackerTransport::Https),
        "udp" => Some(TrackerTransport::Udp),
        "ws" | "wss" => Some(TrackerTransport::WebSocket),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{TrackerTransport, classify_tracker_url, is_udp_tracker_url};

    #[test]
    fn detects_udp_scheme_before_validating_the_url() {
        assert!(is_udp_tracker_url("udp://tracker.example:6969/announce"));
        assert!(is_udp_tracker_url("UDP://"));
        assert!(!is_udp_tracker_url("http://tracker.example/announce"));
    }

    #[test]
    fn classifies_supported_tracker_transports() {
        for (url, expected) in [
            ("http://tracker.example/announce", TrackerTransport::Http),
            ("https://tracker.example/announce", TrackerTransport::Https),
            ("udp://tracker.example:6969/announce", TrackerTransport::Udp),
            ("ws://tracker.example/announce", TrackerTransport::WebSocket),
            (
                "wss://tracker.example/announce",
                TrackerTransport::WebSocket,
            ),
        ] {
            assert_eq!(classify_tracker_url(url), Some(expected), "{url}");
        }
    }

    #[test]
    fn rejects_invalid_and_unsupported_tracker_urls() {
        assert_eq!(classify_tracker_url("not a URL"), None);
        assert_eq!(classify_tracker_url("file:///tracker"), None);
    }
}
