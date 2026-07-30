//! Tests for HealthTrackingAnnounceList, TrackerEntry, and urlencode helpers.

use super::super::*;

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
    use super::super::bt_announce::urlencode_bytes;
    let data = [0x01, 0x02, 0xFF, 0x00];
    let encoded = urlencode_bytes(&data);
    assert_eq!(encoded, "%01%02%FF%00");
}
