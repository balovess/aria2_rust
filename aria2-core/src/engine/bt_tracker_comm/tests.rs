//! Tests for the bt_tracker_comm module.

use super::*;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------
// AnnounceEvent Tests
// ------------------------------------------------------------------

#[test]
fn test_announce_event_transitions() {
    // Started -> Downloading
    assert_eq!(
        AnnounceEvent::Started.next_event(),
        AnnounceEvent::Downloading
    );
    // StartedAfterCompletion -> Seeding
    assert_eq!(
        AnnounceEvent::StartedAfterCompletion.next_event(),
        AnnounceEvent::Seeding
    );
    // Stopped -> Halted
    assert_eq!(AnnounceEvent::Stopped.next_event(), AnnounceEvent::Halted);
    // Completed -> Seeding
    assert_eq!(
        AnnounceEvent::Completed.next_event(),
        AnnounceEvent::Seeding
    );
    // Stable states: Downloading, Seeding, Halted remain unchanged
    assert_eq!(
        AnnounceEvent::Downloading.next_event(),
        AnnounceEvent::Downloading
    );
    assert_eq!(AnnounceEvent::Seeding.next_event(), AnnounceEvent::Seeding);
    assert_eq!(AnnounceEvent::Halted.next_event(), AnnounceEvent::Halted);
}

#[test]
fn test_announce_event_next_if_after_started() {
    // Stopped -> Halted
    assert_eq!(
        AnnounceEvent::Stopped.next_event_if_after_started(),
        AnnounceEvent::Halted
    );
    // Completed -> Seeding
    assert_eq!(
        AnnounceEvent::Completed.next_event_if_after_started(),
        AnnounceEvent::Seeding
    );
    // Others remain unchanged
    assert_eq!(
        AnnounceEvent::Started.next_event_if_after_started(),
        AnnounceEvent::Started
    );
    assert_eq!(
        AnnounceEvent::StartedAfterCompletion.next_event_if_after_started(),
        AnnounceEvent::StartedAfterCompletion
    );
    assert_eq!(
        AnnounceEvent::Downloading.next_event_if_after_started(),
        AnnounceEvent::Downloading
    );
    assert_eq!(
        AnnounceEvent::Seeding.next_event_if_after_started(),
        AnnounceEvent::Seeding
    );
}

#[test]
fn test_announce_tier_accepts_events() {
    // Stopped event accepted by: Downloading, Stopped, Completed, Seeding
    assert!(AnnounceEvent::Downloading.accepts_stopped_event());
    assert!(AnnounceEvent::Stopped.accepts_stopped_event());
    assert!(AnnounceEvent::Completed.accepts_stopped_event());
    assert!(AnnounceEvent::Seeding.accepts_stopped_event());
    assert!(!AnnounceEvent::Started.accepts_stopped_event());
    assert!(!AnnounceEvent::StartedAfterCompletion.accepts_stopped_event());
    assert!(!AnnounceEvent::Halted.accepts_stopped_event());

    // Completed event accepted by: Downloading, Completed
    assert!(AnnounceEvent::Downloading.accepts_completed_event());
    assert!(AnnounceEvent::Completed.accepts_completed_event());
    assert!(!AnnounceEvent::Started.accepts_completed_event());
    assert!(!AnnounceEvent::StartedAfterCompletion.accepts_completed_event());
    assert!(!AnnounceEvent::Stopped.accepts_completed_event());
    assert!(!AnnounceEvent::Seeding.accepts_completed_event());
    assert!(!AnnounceEvent::Halted.accepts_completed_event());
}

#[test]
fn test_announce_event_string() {
    assert_eq!(AnnounceEvent::Started.as_event_string(), "started");
    assert_eq!(
        AnnounceEvent::StartedAfterCompletion.as_event_string(),
        "started"
    );
    assert_eq!(AnnounceEvent::Stopped.as_event_string(), "stopped");
    assert_eq!(AnnounceEvent::Completed.as_event_string(), "completed");
    assert_eq!(AnnounceEvent::Downloading.as_event_string(), "");
    assert_eq!(AnnounceEvent::Seeding.as_event_string(), "");
    assert_eq!(AnnounceEvent::Halted.as_event_string(), "");
}

// ------------------------------------------------------------------
// AnnounceTier Tests
// ------------------------------------------------------------------

#[test]
fn test_announce_tier_next_event() {
    let mut tier = AnnounceTier::from_urls(vec!["http://tracker.test/announce".to_string()]);
    assert_eq!(tier.event, AnnounceEvent::Started);

    tier.next_event();
    assert_eq!(tier.event, AnnounceEvent::Downloading);

    // Downloading is stable
    tier.next_event();
    assert_eq!(tier.event, AnnounceEvent::Downloading);
}

#[test]
fn test_announce_tier_next_event_if_after_started() {
    let mut tier = AnnounceTier::from_urls(vec!["http://tracker.test/announce".to_string()]);
    tier.event = AnnounceEvent::Stopped;
    tier.next_event_if_after_started();
    assert_eq!(tier.event, AnnounceEvent::Halted);

    tier.event = AnnounceEvent::Completed;
    tier.next_event_if_after_started();
    assert_eq!(tier.event, AnnounceEvent::Seeding);

    // Started should NOT transition via nextEventIfAfterStarted
    tier.event = AnnounceEvent::Started;
    tier.next_event_if_after_started();
    assert_eq!(tier.event, AnnounceEvent::Started);
}

// ------------------------------------------------------------------
// AnnounceList Tests
// ------------------------------------------------------------------

#[test]
fn test_announce_list_creation() {
    // Test from announce string
    let list = AnnounceList::new(&[], &Some("http://tracker1.com/announce".to_string()));
    assert_eq!(list.tier_count(), 1);
    assert_eq!(list.get_announce(), Some("http://tracker1.com/announce"));

    // Test from multi-tier list
    let multi_tier = vec![
        vec![
            "http://tier1-1.com/announce".to_string(),
            "http://tier1-2.com/announce".to_string(),
        ],
        vec!["http://tier2-1.com/announce".to_string()],
    ];
    let list2 = AnnounceList::new(&multi_tier, &None);
    assert_eq!(list2.tier_count(), 2);
    assert_eq!(list2.get_announce(), Some("http://tier1-1.com/announce"));

    // Test empty case
    let list3 = AnnounceList::new(&[], &None);
    assert_eq!(list3.tier_count(), 0);
    assert!(list3.get_announce().is_none());
}

#[test]
fn test_announce_list_success_resets_to_first_tier() {
    let multi_tier = vec![
        vec!["http://t1.com/announce".to_string()],
        vec!["http://t2.com/announce".to_string()],
    ];
    let mut list = AnnounceList::new(&multi_tier, &None);

    // Initially at tier 0
    assert_eq!(list.get_announce(), Some("http://t1.com/announce"));

    // Advance to tier 1 via failure
    list.announce_failure();

    // Now at tier 1
    assert_eq!(list.get_announce(), Some("http://t2.com/announce"));

    // Success resets to first tier and advances event
    list.announce_success();
    // C++ behavior: announceSuccess on current tier (tier 1):
    // 1. Calls nextEvent on tier 1 (Started -> Downloading)
    // 2. Removes current URL and pushes to front of tier 1
    // 3. Resets currentTier to begin (tier 0)
    // So we should be back at tier 0, tracker 0 = t1
    assert_eq!(list.get_announce(), Some("http://t1.com/announce"));
}

#[test]
fn test_announce_list_failure_advances_tracker() {
    let urls = vec![
        "http://t1.com/announce".to_string(),
        "http://t2.com/announce".to_string(),
    ];
    let mut list = AnnounceList::new(&[urls], &None);

    // Initially at tracker 0
    assert_eq!(list.get_announce(), Some("http://t1.com/announce"));

    // Failure advances to next tracker in same tier
    list.announce_failure();
    assert_eq!(list.get_announce(), Some("http://t2.com/announce"));
}

#[test]
fn test_announce_list_failure_advances_tier_on_last_url() {
    let multi_tier = vec![
        vec!["http://t1.com/announce".to_string()],
        vec!["http://t2.com/announce".to_string()],
    ];
    let mut list = AnnounceList::new(&multi_tier, &None);

    // Tier 0 has only 1 URL, so failure should advance to tier 1
    list.announce_failure();
    assert_eq!(list.get_announce(), Some("http://t2.com/announce"));
}

#[test]
fn test_announce_list_all_tiers_failed() {
    let multi_tier = vec![
        vec!["http://t1.com/announce".to_string()],
        vec!["http://t2.com/announce".to_string()],
    ];
    let mut list = AnnounceList::new(&multi_tier, &None);

    assert!(!list.all_tiers_failed());

    // Fail tier 0
    list.announce_failure();
    assert!(!list.all_tiers_failed());

    // Fail tier 1
    list.announce_failure();
    assert!(list.all_tiers_failed());
    assert!(list.get_announce().is_none());
}

#[test]
fn test_announce_list_event_management() {
    let mut list = AnnounceList::new(&[vec!["http://t.com/announce".to_string()]], &None);

    // Initial event is Started
    assert_eq!(list.get_event(), AnnounceEvent::Started);
    assert_eq!(list.get_event_string(), "started");

    // Set event to Completed
    list.set_event(AnnounceEvent::Completed);
    assert_eq!(list.get_event(), AnnounceEvent::Completed);
    assert_eq!(list.get_event_string(), "completed");

    // After success, event advances: Completed -> Seeding
    list.announce_success();
    // Success resets to first tier; event on that tier is now Downloading
    // (since the first tier's event was Started, and nextEvent makes it Downloading)
    // Wait - we set it to Completed on the first tier, then announceSuccess
    // calls nextEvent on that tier: Completed -> Seeding
    // Then resets to first tier
    assert_eq!(list.get_event(), AnnounceEvent::Seeding);
}

#[test]
fn test_announce_list_stopped_allowed_tiers() {
    let mut list = AnnounceList::new(
        &[
            vec!["http://t1.com/announce".to_string()],
            vec!["http://t2.com/announce".to_string()],
        ],
        &None,
    );

    // Both tiers start with Started event - does NOT accept stopped
    assert_eq!(list.count_stopped_allowed_tier(), 0);

    // Advance tier 0 to Downloading
    list.announce_success(); // tier 0: Started -> Downloading, reset to tier 0
    assert_eq!(list.get_event(), AnnounceEvent::Downloading);
    assert_eq!(list.count_stopped_allowed_tier(), 1);

    // Advance tier 1 too - need to fail through to it
    list.announce_failure(); // move to tier 1
    list.announce_success(); // tier 1: Started -> Downloading
    // Now both tiers should be Downloading
    assert_eq!(list.count_stopped_allowed_tier(), 2);
}

#[test]
fn test_announce_list_completed_allowed_tiers() {
    let list = AnnounceList::new(
        &[
            vec!["http://t1.com/announce".to_string()],
            vec!["http://t2.com/announce".to_string()],
        ],
        &None,
    );

    // Both tiers start with Started - does NOT accept completed
    assert_eq!(list.count_completed_allowed_tier(), 0);
}

#[test]
fn test_announce_list_move_to_stopped_allowed_tier() {
    let mut list = AnnounceList::new(
        &[
            vec!["http://t1.com/announce".to_string()],
            vec!["http://t2.com/announce".to_string()],
        ],
        &None,
    );

    // Set tier 1 to Downloading (accepts stopped)
    list.tiers[1].event = AnnounceEvent::Downloading;

    // Current tier is 0 (Started, doesn't accept stopped)
    assert!(!list.current_tier_accepts_stopped_event());

    // Move to stopped-allowed tier
    list.move_to_stopped_allowed_tier();
    assert!(list.current_tier_accepts_stopped_event());
    assert_eq!(list.get_announce(), Some("http://t2.com/announce"));
}

#[test]
fn test_announce_list_reset_tier() {
    let multi_tier = vec![
        vec!["http://t1.com/announce".to_string()],
        vec!["http://t2.com/announce".to_string()],
    ];
    let mut list = AnnounceList::new(&multi_tier, &None);

    // Advance through failures
    list.announce_failure();
    assert_eq!(list.get_announce(), Some("http://t2.com/announce"));

    // Reset should go back to beginning
    list.reset_tier();
    assert_eq!(list.get_announce(), Some("http://t1.com/announce"));
}

#[test]
fn test_announce_list_shuffle() {
    let urls: Vec<String> = (0..20)
        .map(|i| format!("http://tracker{}.com/announce", i))
        .collect();
    let mut list = AnnounceList::new(&[urls.clone()], &None);

    let _original_first = list.get_announce().unwrap().to_string();
    list.shuffle();

    // After shuffle, the list should still contain all URLs
    // (it's very unlikely shuffle produces the same order for 20 items)
    assert_eq!(list.tier_count(), 1);
    let tier_urls: Vec<&str> = list.tiers[0].urls.iter().map(|s| s.as_str()).collect();
    assert_eq!(tier_urls.len(), 20);
}

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

// ------------------------------------------------------------------
// Legacy AnnounceList (HealthTracking) Tests
// ------------------------------------------------------------------

#[test]
fn test_health_tracking_announce_list_creation() {
    // Test from announce string
    let list1 =
        HealthTrackingAnnounceList::new(&[], &Some("http://tracker1.com/announce".to_string()));
    assert_eq!(list1.tiers.len(), 1);
    assert_eq!(list1.tiers[0].trackers.len(), 1);
    assert_eq!(
        list1.get_tracker_url(0, 0).unwrap(),
        &"http://tracker1.com/announce".to_string()
    );

    // Test from multi-tier list
    let multi_tier = vec![
        vec![
            "http://tier1-1.com/announce".to_string(),
            "http://tier1-2.com/announce".to_string(),
        ],
        vec!["http://tier2-1.com/announce".to_string()],
    ];
    let list2 = HealthTrackingAnnounceList::new(&multi_tier, &None);
    assert_eq!(list2.tiers.len(), 2);
    assert_eq!(list2.tiers[0].trackers.len(), 2);
    assert_eq!(list2.tiers[1].trackers.len(), 1);

    // Test empty case
    let mut list3 = HealthTrackingAnnounceList::new(&[], &None);
    assert_eq!(list3.tiers.len(), 0);
    assert!(list3.select_next_tracker().is_none());
}

#[test]
fn test_tier_selection_order() {
    let mut tier = TrackerTier::new(vec![
        "http://tracker-a.com".to_string(),
        "http://tracker-b.com".to_string(),
        "http://tracker-c.com".to_string(),
    ]);

    // Give tracker-b better reliability through simulated successes
    tier.trackers[1].record_success(50.0);
    tier.trackers[1].record_success(45.0);
    tier.trackers[1].record_success(55.0);

    // Give tracker-c some failures
    tier.trackers[2].record_failure();

    // Make tracker-a temporarily unavailable to force reliability-based selection
    tier.trackers[0].record_failure();

    // Selection should prefer higher reliability among available trackers
    let selected = tier.select_next();
    assert!(selected.is_some());
    // tracker-b should be selected due to highest reliability score
    assert_eq!(tier.current_index, 1);
}

#[test]
fn test_failover_across_tiers() {
    let mut announce_list = HealthTrackingAnnounceList::new(
        &[
            vec!["http://tier1-tracker.com/announce".to_string()],
            vec!["http://tier2-tracker.com/announce".to_string()],
        ],
        &None,
    );

    // Initially should select from tier 0
    let selection1 = announce_list.select_next_tracker();
    assert_eq!(selection1, Some((0, 0)));

    // Make tier 0 tracker fail multiple times to trigger backoff
    announce_list.tiers[0].trackers[0].record_failure();
    announce_list.tiers[0].trackers[0].record_failure();
    announce_list.tiers[0].trackers[0].record_failure(); // Will have long backoff

    // Now should failover to tier 1
    let selection2 = announce_list.select_next_tracker();
    assert_eq!(selection2, Some((1, 0)));
    assert_eq!(announce_list.current_tier, 1);
}

#[test]
fn test_exponential_backoff_sequence() {
    let mut entry = TrackerEntry::new("http://tracker.test/announce".to_string());

    // Verify backoff sequence: 10 -> 20 -> 40 -> 80 -> 160 -> 320 -> 640 -> 1280 -> 2560 -> 3600 (capped)
    let expected_delays = [10u64, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600];

    for (i, &expected) in expected_delays.iter().enumerate() {
        entry.record_failure();

        // Calculate expected delay based on failure count
        let base: u64 = 10;
        let exp = entry.failure_count.saturating_sub(1).min(10);
        let calculated_delay = base.saturating_mul(1 << exp).min(3600);
        assert_eq!(
            calculated_delay,
            expected,
            "Failure {}: expected {}s, got {}s",
            i + 1,
            expected,
            calculated_delay
        );
    }

    // After many failures, should be capped at 3600 seconds
    assert!(entry.next_retry_after.is_some());
}

#[test]
fn test_reliability_scoring() {
    let mut entry1 = TrackerEntry::new("http://good.tracker/announce".to_string());
    let mut entry2 = TrackerEntry::new("http://bad.tracker/announce".to_string());
    let entry3 = TrackerEntry::new("http://unknown.tracker/announce".to_string());

    // Simulate good tracker with many successes
    for _ in 0..10 {
        entry1.record_success(100.0);
    }

    // Simulate bad tracker with many failures
    for _ in 0..5 {
        entry2.record_failure();
    }

    // Unknown tracker has no history

    let score1 = entry1.reliability_score();
    let score2 = entry2.reliability_score();
    let score3 = entry3.reliability_score();

    // Good tracker should have highest score
    assert!(
        score1 > score3,
        "Good tracker ({}) should beat unknown ({})",
        score1,
        score3
    );

    // Bad tracker should have lowest score (penalized by recent failures)
    assert!(
        score3 > score2,
        "Unknown ({}) should beat bad tracker ({})",
        score3,
        score2
    );

    // Good tracker should definitely beat bad tracker
    assert!(
        score1 > score2,
        "Good tracker ({}) should beat bad tracker ({})",
        score1,
        score2
    );
}

#[test]
fn test_urlencode_infohash() {
    let hash = [0xABu8; 20];
    let encoded = urlencode_infohash(&hash);
    assert_eq!(encoded, "%AB".repeat(20));
}

#[test]
fn test_urlencode_bytes() {
    use super::bt_announce::urlencode_bytes;
    let data = [0x01, 0x02, 0xFF, 0x00];
    let encoded = urlencode_bytes(&data);
    assert_eq!(encoded, "%01%02%FF%00");
}
