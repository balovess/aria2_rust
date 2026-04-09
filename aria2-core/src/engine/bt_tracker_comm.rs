use crate::error::{Aria2Error, RecoverableError, Result};

/// BitTorrent tracker communication module.
///
/// Handles all interactions with:
/// - HTTP/HTTPS trackers (announce requests)
/// - UDP trackers
/// - DHT peer discovery
/// - Public tracker lists for fallback peer discovery
///
/// Extracted from BtDownloadCommand to separate network communication
/// concerns from download orchestration logic.

// ======================================================================
// URL Encoding Helper
// ======================================================================

/// URL-encodes a 20-byte info hash or peer ID for use in tracker URLs.
///
/// Each byte is encoded as `%XX` where XX is the uppercase hex representation.
/// This is required by the BitTorrent tracker protocol specification.
pub fn urlencode_infohash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("%{:02X}", b)).collect()
}

// ======================================================================
// HTTP Tracker Communication
// ======================================================================

/// Announce to a public tracker and collect peer addresses.
///
/// Sends an HTTP GET request to the tracker with standard announce parameters
/// and parses the response to extract peer information.
///
/// # Arguments
/// * `tracker_url` - The announce URL of the public tracker
/// * `info_hash` - 20-byte SHA-1 hash of the torrent's info dictionary
/// * `peer_id` - 20-byte unique identifier for this client
/// * `total_size` - Total size of the torrent content in bytes
///
/// # Returns
/// A vector of `(ip_address, port)` tuples on success.
///
/// # Errors
/// Returns error string if HTTP request fails, response parsing fails,
/// or tracker reports failure.
pub async fn announce_to_public_tracker(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    total_size: u64,
) -> std::result::Result<Vec<(String, u16)>, String> {
    let url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&event=started&compact=1",
        tracker_url,
        urlencode_infohash(info_hash),
        urlencode_infohash(peer_id),
        total_size,
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("build client: {}", e))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("read body: {}", e))?;

    let tracker_resp =
        aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
            .map_err(|e| format!("parse response: {}", e))?;

    if tracker_resp.is_failure() {
        return Err(tracker_resp
            .failure_reason
            .unwrap_or_else(|| "tracker failure".to_string()));
    }

    Ok(tracker_resp
        .peers
        .into_iter()
        .map(|p| (p.ip, p.port))
        .collect())
}

// ======================================================================
// Tracker Peer Discovery Functions
// ======================================================================

/// Perform initial HTTP tracker announce and collect peers.
///
/// This is the first step in peer discovery after torrent metadata is parsed.
/// Sends a "started" event to inform the tracker we're beginning download.
///
/// # Arguments
/// * `announce_url` - The primary tracker announce URL from torrent metadata
/// * `info_hash_raw` - Raw 20-byte info hash
/// * `my_peer_id` - Our 20-byte peer ID
/// * `total_size` - Total torrent size in bytes
///
/// # Returns
/// Vector of peer addresses from the tracker response.
///
/// # Errors
/// Returns error if HTTP request fails, response parsing fails,
/// or tracker indicates failure.
pub async fn perform_http_tracker_announce(
    announce_url: &str,
    info_hash_raw: &[u8; 20],
    my_peer_id: &[u8; 20],
    total_size: u64,
) -> Result<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>> {
    let url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&event=started&compact=1",
        announce_url,
        urlencode_infohash(info_hash_raw),
        urlencode_infohash(my_peer_id),
        total_size,
    );

    eprintln!("[BT] Announcing to tracker: {}", url);
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker HTTP failed: {}", e),
        }))?;
    eprintln!("[BT] Tracker response status: {}", resp.status());
    let body = resp
        .bytes()
        .await
        .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker body read failed: {}", e),
        }))?;
    eprintln!("[BT] Tracker body: {:?}", String::from_utf8_lossy(&body));

    let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(
        &body,
    )
    .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
        message: format!("Tracker parse failed: {}", e),
    }))?;

    eprintln!(
        "[BT] Tracker response: {} peers",
        tracker_resp.peer_count()
    );
    for peer in &tracker_resp.peers {
        eprintln!("[BT]   Peer: {}:{}", peer.ip, peer.port);
    }

    if tracker_resp.is_failure() {
        return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: tracker_resp.failure_reason.unwrap_or_default(),
        }));
    }

    Ok(tracker_resp
        .peers
        .iter()
        .map(|p| aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&p.ip, p.port))
        .collect())
}
