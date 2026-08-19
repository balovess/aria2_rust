//! Tests for BtAnnounce.

use super::super::*;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------
// BtAnnounce Tests
// ------------------------------------------------------------------

#[test]
fn test_bt_announce_default_ready_after_interval() {
    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

    // Initially ready (no previous announce)
    assert!(bt.is_default_announce_ready());

    // Simulate a successful announce
    bt.announce_start();
    assert!(!bt.is_default_announce_ready()); // in-flight

    bt.announce_success();
    // After success, prev_announce_time is set, so not immediately ready
    // (interval hasn't elapsed yet)
    assert!(!bt.is_default_announce_ready());
}

#[test]
fn test_bt_announce_default_ready_when_interval_zero() {
    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
    // Set min_interval to zero so elapsed time check passes
    bt.min_interval = Duration::ZERO;
    bt.prev_announce_time = Some(Instant::now());

    // With zero interval, should be ready immediately after announce
    assert!(bt.is_default_announce_ready());
}

#[test]
fn test_bt_announce_stopped_ready_when_halted() {
    let mut bt = BtAnnounce::new(&[vec!["http://tracker.test/announce".to_string()]], &None);
    // Advance the tier to Downloading so it accepts stopped
    bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

    assert!(!bt.is_stopped_announce_ready()); // not halted

    bt.set_runtime_halted(true);
    assert!(bt.is_stopped_announce_ready());
}

#[test]
fn test_bt_announce_completed_ready() {
    let mut bt = BtAnnounce::new(&[vec!["http://tracker.test/announce".to_string()]], &None);
    // Advance the tier to Downloading so it accepts completed
    bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

    assert!(!bt.is_completed_announce_ready()); // not complete

    bt.set_download_complete(true);
    assert!(bt.is_completed_announce_ready());
}

#[test]
fn test_bt_announce_no_more_announce() {
    let mut bt = BtAnnounce::new(&[vec!["http://tracker.test/announce".to_string()]], &None);

    assert!(!bt.no_more_announce()); // not halted

    bt.set_runtime_halted(true);
    // Tier 0 is still Started, doesn't accept stopped
    assert!(bt.no_more_announce());

    // Advance tier to Downloading (accepts stopped)
    bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;
    assert!(!bt.no_more_announce());
}

#[test]
fn test_bt_announce_adjust_started_after_completion() {
    let mut bt = BtAnnounce::new(&[vec!["http://tracker.test/announce".to_string()]], &None);

    // Mark download complete while event is still Started
    bt.set_download_complete(true);
    // Override min_interval so default announce is ready
    bt.min_interval = Duration::ZERO;
    bt.prev_announce_time = Some(Instant::now());

    assert!(bt.adjust_announce_list());
    // Event should be changed to STARTED_AFTER_COMPLETION
    assert_eq!(
        bt.announce_list().get_event(),
        AnnounceEvent::StartedAfterCompletion
    );
}

#[test]
fn test_bt_announce_adjust_stopped_priority() {
    let mut bt = BtAnnounce::new(&[vec!["http://tracker.test/announce".to_string()]], &None);

    // Set up both stopped and completed as ready
    bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;
    bt.set_runtime_halted(true);
    bt.set_download_complete(true);
    bt.min_interval = Duration::ZERO;
    bt.prev_announce_time = Some(Instant::now());

    assert!(bt.adjust_announce_list());
    // Stopped should take priority over completed
    assert_eq!(bt.announce_list().get_event(), AnnounceEvent::Stopped);
}

#[test]
fn test_get_announce_url_includes_all_params() {
    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
    bt.set_tcp_port(6881);

    let info_hash = [0xABu8; 20];
    let peer_id = [0xCDu8; 20];
    let url = bt
        .get_announce_url(&info_hash, &peer_id, 1000, 5000, 500, None)
        .unwrap();

    // Verify all required parameters are present
    assert!(url.contains("info_hash="));
    assert!(url.contains("peer_id="));
    assert!(url.contains("uploaded=1000"));
    assert!(url.contains("downloaded=5000"));
    assert!(url.contains("left=500"));
    assert!(url.contains("compact=1"));
    assert!(url.contains("key="));
    assert!(url.contains("numwant="));
    assert!(url.contains("no_peer_id=1"));
    assert!(url.contains("port=6881"));
    assert!(url.contains("event=started"));
    assert!(url.contains("supportcrypto=1"));
}

#[test]
fn test_get_announce_url_with_existing_query() {
    let mut bt = BtAnnounce::new(
        &[],
        &Some("http://tracker.test/announce?passkey=abc123".to_string()),
    );

    let info_hash = [0u8; 20];
    let peer_id = [0u8; 20];
    let url = bt
        .get_announce_url(&info_hash, &peer_id, 0, 0, 0, None)
        .unwrap();

    // Should use & instead of ? when URL already has query params
    assert!(url.contains("&info_hash="));
    assert!(!url.contains("?info_hash="));
}

#[test]
fn test_numwant_matches_runtime_peer_state() {
    let mut bt = BtAnnounce::new(&[], &Some("udp://tracker.test:6969".to_string()));
    assert_eq!(bt.numwant(), 50);

    bt.set_less_than_min_peers(false);
    assert_eq!(bt.numwant(), 0);

    bt.set_less_than_min_peers(true);
    bt.set_runtime_halted(true);
    assert_eq!(bt.numwant(), 0);

    bt.set_runtime_halted(false);
    assert_eq!(bt.numwant(), 50);
}

#[test]
fn test_get_announce_url_numwant_zero_when_enough_peers() {
    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
    bt.set_less_than_min_peers(false); // we have enough peers

    let info_hash = [0u8; 20];
    let peer_id = [0u8; 20];
    let url = bt
        .get_announce_url(&info_hash, &peer_id, 0, 0, 0, None)
        .unwrap();

    assert!(url.contains("numwant=0"));
}

#[test]
fn test_current_udp_event_mapping_matches_wire_values() {
    use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent;

    let mut bt = BtAnnounce::new(&[], &Some("udp://tracker.test:6969".to_string()));
    for (event, expected) in [
        (AnnounceEvent::Downloading, UdpEvent::None),
        (AnnounceEvent::Started, UdpEvent::Started),
        (AnnounceEvent::StartedAfterCompletion, UdpEvent::Started),
        (AnnounceEvent::Completed, UdpEvent::Completed),
        (AnnounceEvent::Stopped, UdpEvent::Stopped),
        (AnnounceEvent::Seeding, UdpEvent::None),
        (AnnounceEvent::Halted, UdpEvent::None),
    ] {
        bt.announce_list_mut().tiers[0].event = event;
        assert_eq!(bt.current_udp_event(), expected);
    }
}

#[test]
fn test_get_announce_url_numwant_zero_when_halted() {
    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
    bt.set_runtime_halted(true);
    bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

    let info_hash = [0u8; 20];
    let peer_id = [0u8; 20];
    let url = bt
        .get_announce_url(&info_hash, &peer_id, 0, 0, 0, None)
        .unwrap();

    assert!(url.contains("numwant=0"));
}

#[test]
fn test_process_announce_response_updates_interval() {
    use aria2_protocol::bittorrent::tracker::response::{PeerInfo, TrackerResponse};

    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

    let response = TrackerResponse {
        interval: 900,
        min_interval: Some(300),
        seeders: 10,
        leechers: 5,
        peers: vec![PeerInfo {
            ip: "1.2.3.4".to_string(),
            port: 6881,
            peer_id: None,
        }],
        peers6: vec![],
        tracker_id: None,
        warning_message: None,
        failure_reason: None,
    };

    let result = bt.process_announce_response(&response);
    assert!(result.is_ok());
    assert_eq!(bt.interval(), Duration::from_secs(900));
    assert_eq!(bt.min_interval(), Duration::from_secs(300));
    assert_eq!(bt.complete(), 10);
    assert_eq!(bt.incomplete(), 5);
    assert_eq!(result.unwrap().len(), 1);
}

#[test]
fn test_process_announce_response_failure() {
    use aria2_protocol::bittorrent::tracker::response::TrackerResponse;

    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

    let response = TrackerResponse {
        interval: 300,
        min_interval: None,
        seeders: 0,
        leechers: 0,
        peers: vec![],
        peers6: vec![],
        tracker_id: None,
        warning_message: None,
        failure_reason: Some("tracker offline".to_string()),
    };

    let result = bt.process_announce_response(&response);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "tracker offline");
}

#[test]
fn test_process_announce_response_min_interval_capped() {
    use aria2_protocol::bittorrent::tracker::response::TrackerResponse;

    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

    // min_interval > interval should be capped
    let response = TrackerResponse {
        interval: 300,
        min_interval: Some(600),
        seeders: 0,
        leechers: 0,
        peers: vec![],
        peers6: vec![],
        tracker_id: None,
        warning_message: None,
        failure_reason: None,
    };

    let _ = bt.process_announce_response(&response);
    assert_eq!(bt.interval(), Duration::from_secs(300));
    assert_eq!(bt.min_interval(), Duration::from_secs(300)); // capped to interval
}

#[test]
fn test_process_announce_response_uses_interval_as_min() {
    use aria2_protocol::bittorrent::tracker::response::TrackerResponse;

    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

    // No min_interval: should use interval as min_interval
    let response = TrackerResponse {
        interval: 600,
        min_interval: None,
        seeders: 0,
        leechers: 0,
        peers: vec![],
        peers6: vec![],
        tracker_id: None,
        warning_message: None,
        failure_reason: None,
    };

    let _ = bt.process_announce_response(&response);
    assert_eq!(bt.interval(), Duration::from_secs(600));
    assert_eq!(bt.min_interval(), Duration::from_secs(600)); // same as interval
}

#[test]
fn test_process_announce_response_stores_tracker_id() {
    use aria2_protocol::bittorrent::tracker::response::{PeerInfo, TrackerResponse};

    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

    // Process response with tracker_id
    let response = TrackerResponse {
        interval: 300,
        min_interval: None,
        seeders: 0,
        leechers: 0,
        peers: vec![PeerInfo {
            ip: "1.2.3.4".to_string(),
            port: 6881,
            peer_id: None,
        }],
        peers6: vec![],
        tracker_id: Some("tracker-abc".to_string()),
        warning_message: None,
        failure_reason: None,
    };

    let _ = bt.process_announce_response(&response);
    assert_eq!(bt.tracker_id(), "tracker-abc");

    // Verify tracker_id is sent in subsequent announce URLs
    let info_hash = [0u8; 20];
    let peer_id = [0u8; 20];
    let url = bt
        .get_announce_url(&info_hash, &peer_id, 0, 0, 0, None)
        .unwrap();
    assert!(
        url.contains("&trackerid="),
        "announce URL should contain trackerid parameter: {}",
        url
    );
}

#[test]
fn test_bt_announce_user_defined_interval() {
    let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
    bt.prev_announce_time = Some(Instant::now());

    // With normal min_interval (120s), not ready
    assert!(!bt.is_default_announce_ready());

    // Set user-defined interval to 0 (use tracker interval)
    bt.set_user_defined_interval(Duration::ZERO);
    assert!(!bt.is_default_announce_ready());

    // Set user-defined interval to 0 seconds (immediate)
    bt.set_user_defined_interval(Duration::from_secs(0));
    // Zero duration means use tracker interval, so still not ready
    // Actually Duration::ZERO == Duration::from_secs(0), which is > Duration::ZERO is false
    // So the check `user_defined_interval > Duration::ZERO` is false, so it uses min_interval
    assert!(!bt.is_default_announce_ready());

    // Override min_interval to 0
    bt.override_min_interval(Duration::ZERO);
    assert!(bt.is_default_announce_ready());
}
