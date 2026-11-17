//! HTTP tracker announce URL encoding and query construction.

/// Inputs used to build an HTTP tracker announce URL.
pub struct HttpAnnounceRequest<'a> {
    /// Tracker announce URL before this request's query parameters.
    pub base_url: &'a str,
    /// Torrent v1 info hash.
    pub info_hash: &'a [u8; 20],
    /// Local BitTorrent peer ID.
    pub peer_id: &'a [u8; 20],
    /// Total uploaded bytes.
    pub uploaded: u64,
    /// Total downloaded bytes.
    pub downloaded: u64,
    /// Bytes remaining to download.
    pub left: u64,
    /// Explicit tracker key, or `None` to use the last eight peer ID bytes.
    pub key: Option<&'a [u8]>,
    /// Desired number of peers.
    pub num_want: u32,
    /// Local TCP listening port; zero omits the `port` parameter.
    pub port: u16,
    /// Optional announce event such as `started` or `completed`.
    pub event: Option<&'a str>,
    /// Optional tracker ID returned by a previous response.
    pub tracker_id: Option<&'a str>,
    /// Whether the client requires encrypted peer connections.
    pub require_crypto: bool,
    /// Optional external peer IP address reported to the tracker.
    pub external_ip: Option<&'a str>,
}

/// Build a tracker URL with the BitTorrent HTTP announce query parameters.
///
/// The existing query string, if present, is retained and new parameters are
/// appended in the established aria2-compatible order.
pub fn build_announce_url(request: &HttpAnnounceRequest<'_>) -> String {
    let separator = if request.base_url.contains('?') {
        "&"
    } else {
        "?"
    };
    let key = request
        .key
        .unwrap_or(&request.peer_id[request.peer_id.len() - 8..]);
    let mut url = format!(
        "{}{}info_hash={}&peer_id={}&uploaded={}&downloaded={}&left={}&compact=1&key={}&numwant={}&no_peer_id=1",
        request.base_url,
        separator,
        urlencode_bytes(request.info_hash),
        urlencode_bytes(request.peer_id),
        request.uploaded,
        request.downloaded,
        request.left,
        urlencode_bytes(key),
        request.num_want,
    );

    if request.port != 0 {
        url.push_str(&format!("&port={}", request.port));
    }
    if let Some(event) = request.event.filter(|event| !event.is_empty()) {
        url.push_str("&event=");
        url.push_str(event);
    }
    if let Some(tracker_id) = request
        .tracker_id
        .filter(|tracker_id| !tracker_id.is_empty())
    {
        url.push_str("&trackerid=");
        url.push_str(&urlencode_bytes(tracker_id.as_bytes()));
    }
    if request.require_crypto {
        url.push_str("&requirecrypto=1");
    } else {
        url.push_str("&supportcrypto=1");
    }
    if let Some(external_ip) = request.external_ip {
        url.push_str("&ip=");
        url.push_str(external_ip);
    }

    url
}

/// Percent-encode bytes using uppercase hexadecimal, as tracker queries expect.
pub fn urlencode_bytes(data: &[u8]) -> String {
    data.iter().map(|byte| format!("%{byte:02X}")).collect()
}

/// Percent-encode a 20-byte info hash or peer ID for an HTTP tracker URL.
pub fn urlencode_infohash(hash: &[u8; 20]) -> String {
    urlencode_bytes(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_announce_query_with_existing_query_and_encoded_binary_fields() {
        let url = build_announce_url(&HttpAnnounceRequest {
            base_url: "https://tracker.example/announce?pass=abc",
            info_hash: &[0xAB; 20],
            peer_id: &[0xCD; 20],
            uploaded: 5,
            downloaded: 10,
            left: 20,
            key: Some(&[0x01, 0xFF]),
            num_want: 50,
            port: 6881,
            event: Some("started"),
            tracker_id: Some("tracker id"),
            require_crypto: true,
            external_ip: Some("192.0.2.1"),
        });

        assert!(url.starts_with("https://tracker.example/announce?pass=abc&info_hash="));
        assert!(url.contains(&format!("info_hash={}", "%AB".repeat(20))));
        assert!(url.contains(&format!("peer_id={}", "%CD".repeat(20))));
        assert!(url.contains("key=%01%FF"));
        assert!(url.contains("&port=6881&event=started&trackerid=%74%72%61%63%6B%65%72%20%69%64"));
        assert!(url.ends_with("&requirecrypto=1&ip=192.0.2.1"));
    }

    #[test]
    fn omits_empty_optional_values_and_uses_peer_id_suffix_as_key() {
        let peer_id = [0x42; 20];
        let url = build_announce_url(&HttpAnnounceRequest {
            base_url: "http://tracker.example/announce",
            info_hash: &[0; 20],
            peer_id: &peer_id,
            uploaded: 0,
            downloaded: 0,
            left: 0,
            key: None,
            num_want: 0,
            port: 0,
            event: Some(""),
            tracker_id: Some(""),
            require_crypto: false,
            external_ip: None,
        });

        assert!(url.contains(&format!("key={}", "%42".repeat(8))));
        assert!(!url.contains("&port="));
        assert!(!url.contains("&event="));
        assert!(!url.contains("&trackerid="));
        assert!(url.ends_with("&supportcrypto=1"));
    }
}
