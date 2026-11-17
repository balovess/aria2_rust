//! Comprehensive test suite for the choking algorithm

use super::*;
use std::net::SocketAddr;

/// Helper to create a test peer with specific characteristics
fn create_test_peer(
    download_speed: f64,
    upload_speed: f64,
    am_choking: bool,
    peer_interested: bool,
) -> PeerStats {
    let addr: SocketAddr = format!(
        "127.0.0.1:{}",
        6881u32.saturating_add(download_speed as u32).min(65535)
    )
    .parse()
    .unwrap();
    let mut peer_id = [0u8; 20];
    peer_id[..8].copy_from_slice(&download_speed.to_bits().to_be_bytes());
    let mut peer = PeerStats::new(peer_id, addr);
    peer.download_speed = download_speed;
    peer.upload_speed = upload_speed;
    peer.am_choking = am_choking;
    peer.peer_interested = peer_interested;
    peer
}

#[path = "tests/identity_snubbing.rs"]
mod identity_snubbing;
#[path = "tests/scoring_rotation.rs"]
mod scoring_rotation;
#[path = "tests/selection.rs"]
mod selection;
