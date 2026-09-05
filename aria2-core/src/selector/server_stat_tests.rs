// Tests for ServerStat (extracted to keep main file under 600 lines).

use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

#[test]
fn test_creation() {
    let stat = ServerStat::new("example.com");
    assert_eq!(stat.host.as_ref(), "example.com");
    assert_eq!(stat.get_download_speed(), 0);
    assert_eq!(stat.get_single_avg_speed(), 0);
    assert!(stat.is_ok());
    assert_eq!(stat.get_counter(), 0);
}

#[test]
fn test_update_single_speed() {
    let stat = ServerStat::new("example.com");
    stat.update_speed(1000, false);
    assert_eq!(stat.get_download_speed(), 1000);
    assert_eq!(stat.get_single_avg_speed(), 700); // 0*0.3 + 1000*0.7

    stat.update_speed(2000, false);
    assert_eq!(stat.get_single_avg_speed(), 1610); // 700*0.3 + 2000*0.7
}

#[test]
fn test_update_multi_speed_independent() {
    let stat = ServerStat::new("example.com");
    stat.update_speed(1000, true);
    assert_eq!(stat.get_multi_avg_speed(), 700);
    assert_eq!(stat.get_single_avg_speed(), 0);

    stat.update_speed(500, false);
    assert_eq!(stat.get_single_avg_speed(), 350);
    assert_eq!(stat.get_multi_avg_speed(), 700);
}

#[test]
fn test_get_avg_speed_combines_both() {
    let stat = ServerStat::new("example.com");
    stat.update_speed(1000, false);
    stat.update_speed(2000, true);
    let avg = stat.get_avg_speed();
    assert!(avg > 0);
    assert!((350..=1400).contains(&avg));
}

#[test]
fn test_status_toggle() {
    let stat = ServerStat::new("example.com");
    assert!(stat.is_ok());

    stat.set_error();
    assert!(!stat.is_ok());

    stat.reset_status();
    assert!(stat.is_ok());
}

#[test]
fn test_counter_operations() {
    let stat = ServerStat::new("example.com");
    assert_eq!(stat.get_counter(), 0);

    let c1 = stat.increment_counter();
    assert_eq!(c1, 1);
    assert_eq!(stat.get_counter(), 1);

    let c2 = stat.increment_counter();
    assert_eq!(c2, 2);

    stat.reset_counter();
    assert_eq!(stat.get_counter(), 0);
}

#[test]
fn test_is_fresh_after_update() {
    let stat = ServerStat::new("example.com");
    assert!(!stat.is_fresh(60));

    stat.update_speed(1000, false);
    assert!(stat.is_fresh(60));
    assert!(!stat.is_fresh(0));
}

#[test]
fn test_concurrent_updates() {
    let stat = Arc::new(ServerStat::new("concurrent.test"));
    let mut handles = Vec::new();

    for i in 0..10u64 {
        let s = Arc::clone(&stat);
        handles.push(thread::spawn(move || {
            s.update_speed((i + 1) * 1000, i % 2 == 0);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert!(stat.get_download_speed() > 0);
    assert!(stat.is_fresh(60));
}

// ======================================================================
// Tests for Availability Cooldown
// ======================================================================

#[test]
fn test_server_available_initially() {
    let stat = ServerStat::new("fresh.server");
    assert!(stat.is_available(), "New server should be available");
}

#[test]
fn test_server_available_with_few_failures() {
    let mut stat = ServerStat::new("some.failures");
    stat.consecutive_failures = 2;
    stat.last_error_time = Some(SystemTime::now());
    assert!(
        stat.is_available(),
        "Server with <3 failures should still be available"
    );
}

#[test]
fn test_server_unavailable_after_3_failures() {
    let mut stat = ServerStat::new("cooldown.server");
    stat.consecutive_failures = 3;
    stat.last_error_time = Some(SystemTime::now());
    assert!(
        !stat.is_available(),
        "Server with 3+ recent failures should be unavailable"
    );
}

#[test]
fn test_server_available_after_cooldown_expires() {
    let mut stat = ServerStat::new("recovered.server");
    stat.consecutive_failures = 5;
    stat.last_error_time = Some(SystemTime::now() - std::time::Duration::from_secs(61));
    assert!(
        stat.is_available(),
        "Server should become available after cooldown expires"
    );
}

#[test]
fn test_set_failure_info() {
    let mut stat = ServerStat::new("failure.test");

    assert_eq!(stat.get_consecutive_failures(), 0);
    assert_eq!(stat.get_last_error_code(), 0);
    assert_eq!(stat.get_last_error_time(), 0);

    stat.set_failure_info(500);

    assert_eq!(stat.get_consecutive_failures(), 1);
    assert_eq!(stat.get_last_error_code(), 500);
    assert!(stat.get_last_error_time() > 0);

    stat.set_failure_info(503);
    assert_eq!(stat.get_consecutive_failures(), 2);
    assert_eq!(stat.get_last_error_code(), 503);
}

// ======================================================================
// Tests for Persistence (Snapshot Roundtrip)
// ======================================================================

#[test]
fn test_snapshot_roundtrip_basic() {
    let stat = ServerStat::new("snapshot.test");
    stat.update_speed(5000, false);
    stat.update_speed(10000, true);
    stat.increment_counter();
    stat.increment_counter();

    let snapshot = stat.to_snapshot();
    let restored = ServerStat::from_snapshot(&snapshot);

    assert_eq!(restored.host.as_ref(), "snapshot.test");
    assert_eq!(restored.get_download_speed(), 10000);
    assert_eq!(restored.get_counter(), 2);
    assert!(restored.get_single_avg_speed() > 0);
    assert!(restored.get_multi_avg_speed() > 0);
}

#[test]
fn test_snapshot_roundtrip_with_failures() {
    let mut stat = ServerStat::new("failed.snapshot.test");
    stat.update_speed(3000, false);
    stat.set_failure_info(500);
    stat.set_failure_info(503);

    let snapshot = stat.to_snapshot();
    assert_eq!(snapshot.consecutive_failures, 2);
    assert_eq!(snapshot.last_error_code, 503);
    assert!(snapshot.last_error_time.is_some());

    let restored = ServerStat::from_snapshot(&snapshot);
    assert_eq!(restored.get_consecutive_failures(), 2);
    assert_eq!(restored.get_last_error_code(), 503);
    assert!(restored.get_last_error_time() > 0);
}

#[test]
fn test_snapshot_preserves_all_fields() {
    let mut stat = ServerStat::new("complete.snapshot.test");
    stat.update_speed(12345, false);
    stat.update_speed(67890, true);
    for _ in 0..5 {
        stat.increment_counter();
    }
    stat.set_error();
    stat.set_failure_info(502);

    let snapshot = stat.to_snapshot();

    assert_eq!(snapshot.host, "complete.snapshot.test");
    assert_eq!(snapshot.download_speed, 67890);
    assert!(snapshot.single_connection_avg_speed > 0);
    assert!(snapshot.multi_connection_avg_speed > 0);
    assert!(snapshot.last_updated > 0);
    assert_eq!(snapshot.status, 1); // Error status
    assert_eq!(snapshot.counter, 5);
    assert!(snapshot.last_error_time.is_some());
    assert_eq!(snapshot.last_error_code, 502);
    assert_eq!(snapshot.consecutive_failures, 1);

    let restored = ServerStat::from_snapshot(&snapshot);
    assert_eq!(restored.host.as_ref(), snapshot.host);
    assert_eq!(restored.get_download_speed(), snapshot.download_speed);
    assert_eq!(
        restored.get_single_avg_speed(),
        snapshot.single_connection_avg_speed
    );
    assert_eq!(
        restored.get_multi_avg_speed(),
        snapshot.multi_connection_avg_speed
    );
    assert_eq!(restored.get_counter(), snapshot.counter);
    assert!(!restored.is_ok());
}

#[test]
fn test_snapshot_json_serialization() {
    let stat = ServerStat::new("json.test");
    stat.update_speed(10000, false);
    stat.increment_counter();

    let snapshot = stat.to_snapshot();

    let json = serde_json::to_string(&snapshot).expect("Should serialize to JSON");
    assert!(json.contains("json.test"));
    assert!(json.contains("10000"));

    let deserialized: ServerStatSnapshot =
        serde_json::from_str(&json).expect("Should deserialize from JSON");
    assert_eq!(deserialized.host, "json.test");
    assert_eq!(deserialized.download_speed, 10000);
    assert_eq!(deserialized.counter, 1);
}

#[test]
fn test_snapshot_zero_values() {
    let stat = ServerStat::new("zero.test");

    let snapshot = stat.to_snapshot();
    assert_eq!(snapshot.download_speed, 0);
    assert_eq!(snapshot.single_connection_avg_speed, 0);
    assert_eq!(snapshot.multi_connection_avg_speed, 0);
    assert_eq!(snapshot.counter, 0);
    assert_eq!(snapshot.status, 0);
    assert!(snapshot.last_error_time.is_none());
    assert_eq!(snapshot.last_error_code, 0);
    assert_eq!(snapshot.consecutive_failures, 0);

    let restored = ServerStat::from_snapshot(&snapshot);
    assert_eq!(restored.get_download_speed(), 0);
    assert!(restored.is_ok());
}
