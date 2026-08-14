//! HTTP/HTTPS tracker announce free functions.
//!
//! Contains the public async functions for announcing to HTTP/HTTPS trackers,
//! including simple public-tracker helpers and the full state-machine-aware
//! announce functions used by [`super::BtAnnounce`].

use super::tracker_url::urlencode_infohash;
use crate::engine::http_tracker_client::{TrackerEvent, build_tracker_client, is_https_tracker};
use crate::error::{Aria2Error, RecoverableError, Result};
use tracing::{debug, info};

/// Tracker request timeout (seconds)
const TRACKER_REQUEST_TIMEOUT_SECS: u64 = 5;

// ======================================================================
// Simple Public Tracker Announce
// ======================================================================

/// Announce to a public tracker and collect peer addresses.
///
/// Sends an HTTP/HTTPS GET request to the tracker with standard announce parameters
/// and parses the response to extract peer information.
///
/// This function automatically detects HTTPS URLs and uses the Rustls
/// transport when required. It is a standalone helper without a
/// `DownloadOptions` owner, so the production command path uses
/// `TrackerAnnouncer` when per-download TLS settings are available.
///
/// # Arguments
/// * `tracker_url` - The announce URL of the public tracker (http:// or https://)
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
    announce_to_public_tracker_with_event(
        tracker_url,
        info_hash,
        peer_id,
        total_size,
        TrackerEvent::Started, // Default event type
    )
    .await
}

/// Announce to a public tracker with explicit event control.
///
/// Extended version of [`announce_to_public_tracker`] that accepts a specific
/// [`TrackerEvent`] for state machine integration.
///
/// # Arguments
/// * `tracker_url` - The announce URL of the public tracker
/// * `info_hash` - 20-byte SHA-1 hash of the torrent's info dictionary
/// * `peer_id` - 20-byte unique identifier for this client
/// * `total_size` - Total size of the torrent content in bytes
/// * `event` - The tracker event to send
pub async fn announce_to_public_tracker_with_event(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    total_size: u64,
    event: TrackerEvent,
) -> std::result::Result<Vec<(String, u16)>, String> {
    // Detect HTTPS scheme for logging and configuration purposes
    let is_https = is_https_tracker(tracker_url);
    if is_https {
        debug!("HTTPS tracker detected: {} (using native-tls)", tracker_url);
    }

    let event_param = if event == TrackerEvent::None {
        String::new()
    } else {
        format!("&event={}", event.as_str())
    };

    let url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}{}&compact=1",
        tracker_url,
        urlencode_infohash(info_hash),
        urlencode_infohash(peer_id),
        total_size,
        event_param,
    );

    let client = build_tracker_client(TRACKER_REQUEST_TIMEOUT_SECS)
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

    let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
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
// Full State-Machine Tracker Announce
// ======================================================================

/// Perform initial HTTP tracker announce and collect peers.
///
/// This is the first step in peer discovery after torrent metadata is parsed.
/// Sends a "started" event to inform the tracker we're beginning download.
///
/// Automatically detects HTTPS URLs and uses TLS when required.
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
/// Perform an announce with a specific tracker event (for state machine integration).
///
/// Use this for sending Completed and Stopped events at appropriate lifecycle points.
pub async fn perform_announce_with_event(
    announce_url: &str,
    info_hash_raw: &[u8; 20],
    my_peer_id: &[u8; 20],
    downloaded: u64,
    left: u64,
    uploaded: u64,
    event: TrackerEvent,
) -> Result<()> {
    let is_https = is_https_tracker(announce_url);

    let event_str = event.as_str();
    let event_param = if event_str.is_empty() {
        String::new()
    } else {
        format!("&event={}", event_str)
    };

    let url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded={}&downloaded={}&left={}&{}compact=1",
        announce_url,
        urlencode_infohash(info_hash_raw),
        urlencode_infohash(my_peer_id),
        uploaded,
        downloaded,
        left,
        event_param,
    );

    info!(
        "[BT] Announce to {} (event={}, https={})",
        announce_url, event_str, is_https
    );

    let client = build_tracker_client(TRACKER_REQUEST_TIMEOUT_SECS).map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Failed to build tracker client: {}", e),
        })
    })?;

    let resp = client.get(&url).send().await.map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker HTTP failed: {}", e),
        })
    })?;

    let body = resp.bytes().await.map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker body read failed: {}", e),
        })
    })?;

    let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Tracker parse failed: {}", e),
            })
        })?;

    if tracker_resp.is_failure() {
        return Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: tracker_resp.failure_reason.unwrap_or_default(),
            },
        ));
    }

    info!("[BT] Announce success (event={})", event_str);
    Ok(())
}
