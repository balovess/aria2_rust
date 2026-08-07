//! Tests for AnnounceEvent, AnnounceTier, and AnnounceList.

use super::super::*;

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
    let mut list = AnnounceList::new(std::slice::from_ref(&urls), &None);

    let _original_first = list.get_announce().unwrap().to_string();
    list.shuffle();

    // After shuffle, the list should still contain all URLs
    // (it's very unlikely shuffle produces the same order for 20 items)
    assert_eq!(list.tier_count(), 1);
    let tier_urls: Vec<&str> = list.tiers[0].urls.iter().map(|s| s.as_str()).collect();
    assert_eq!(tier_urls.len(), 20);
}
