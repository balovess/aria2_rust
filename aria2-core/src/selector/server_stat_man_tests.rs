// Tests for ServerStatMan (extracted to keep main file under 600 lines).

#[test]
fn test_creation_and_count() {
    let man = ServerStatMan::new();
    assert_eq!(man.count(), 0);
}

#[test]
fn test_get_or_create_new_host() {
    let man = ServerStatMan::new();
    let stat = man.get_or_create("example.com");
    assert_eq!(stat.host, "example.com");
    assert_eq!(man.count(), 1);
}

#[test]
fn test_get_or_create_returns_same_instance() {
    let man = ServerStatMan::new();
    let s1 = man.get_or_create("example.com");
    let s2 = man.get_or_create("example.com");
    assert!(Arc::ptr_eq(&s1, &s2));
    assert_eq!(man.count(), 1);
}

#[test]
fn test_find_existing() {
    let man = ServerStatMan::new();
    man.get_or_create("example.com");
    assert!(man.find_stat("example.com").is_some());
    assert!(man.find_stat("nonexistent").is_none());
}

#[test]
fn test_update_creates_if_needed() {
    let man = ServerStatMan::new();
    man.update("fast.server", 5000, false);
    assert_eq!(man.count(), 1);

    let stat = man.find_stat("fast.server").unwrap();
    assert_eq!(stat.get_download_speed(), 5000);
}

#[test]
fn test_remove() {
    let man = ServerStatMan::new();
    man.get_or_create("a.com");
    man.get_or_create("b.com");
    assert_eq!(man.count(), 2);

    man.remove("a.com");
    assert_eq!(man.count(), 1);
    assert!(man.find_stat("a.com").is_none());
}

#[test]
fn test_multiple_hosts_independent() {
    let man = ServerStatMan::new();
    man.update("fast.com", 10000, true);
    man.update("slow.com", 100, false);

    let fast = man.find_stat("fast.com").unwrap();
    let slow = man.find_stat("slow.com").unwrap();

    assert_ne!(fast.get_avg_speed(), slow.get_avg_speed());
    assert!(fast.get_avg_speed() > slow.get_avg_speed());
}

#[test]
fn test_hosts_list() {
    let man = ServerStatMan::new();
    man.get_or_create("alpha.com");
    man.get_or_create("beta.com");
    let hosts = man.hosts();
    assert_eq!(hosts.len(), 2);
    assert!(hosts.contains(&"alpha.com".to_string()));
    assert!(hosts.contains(&"beta.com".to_string()));
}

// ======================================================================
// Tests for mark_failure
// ======================================================================

#[test]
fn test_mark_failure_updates_stats() {
    let man = ServerStatMan::new();
    man.get_or_create("failing.host");

    man.mark_failure("failing.host", 500);

    let stat = man.find_stat("failing.host").unwrap();
    assert_eq!(stat.get_consecutive_failures(), 1);
    assert!(stat.get_last_error_time() > 0);
    assert_eq!(stat.get_last_error_code(), 500);
}

#[test]
fn test_mark_failure_multiple_times() {
    let man = ServerStatMan::new();
    man.get_or_create("repeated.failures");

    for i in 0..5u16 {
        man.mark_failure("repeated.failures", i);
    }

    let stat = man.find_stat("repeated.failures").unwrap();
    assert_eq!(stat.get_consecutive_failures(), 5);
    assert!(
        !stat.is_available(),
        "Should be unavailable after 5 failures"
    );
}

#[test]
fn test_mark_failure_nonexistent_host() {
    let man = ServerStatMan::new();
    man.mark_failure("nonexistent.host", 404);
    assert_eq!(man.count(), 0);
}

// ======================================================================
// Tests for Persistence (save/load)
// ======================================================================

#[test]
fn test_save_to_file_basic() {
    let man = ServerStatMan::new();
    man.update("fast.mirror.com", 10000, false);
    man.update("slow.mirror.com", 100, true);

    let temp_file = std::env::temp_dir().join("test_server_stat_save.json");
    let saved = man.save_to_file(&temp_file).expect("Save should succeed");

    assert_eq!(saved, 2, "Should save 2 servers");

    let content = std::fs::read_to_string(&temp_file).expect("Should read file");
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("Should be valid JSON");
    assert!(parsed.get("version").is_some());
    assert!(parsed.get("saved_at").is_some());
    assert!(parsed.get("servers").is_some());

    let _ = std::fs::remove_file(&temp_file);
}

#[test]
fn test_load_from_file_basic() {
    let man = ServerStatMan::new();
    man.update("load.test.com", 5000, false);
    man.mark_failure("load.test.com", 500);

    let temp_file = std::env::temp_dir().join("test_server_stat_load.json");
    let _ = man.save_to_file(&temp_file);

    let man2 = ServerStatMan::new();
    let loaded = man2
        .load_from_file(&temp_file)
        .expect("Load should succeed");

    assert_eq!(loaded, 1, "Should load 1 server");

    let stat = man2
        .find_stat("load.test.com")
        .expect("Should find loaded server");
    assert!(stat.get_avg_speed() > 0);
    assert_eq!(stat.get_consecutive_failures(), 1);

    let _ = std::fs::remove_file(&temp_file);
}

#[test]
fn test_save_load_roundtrip() {
    let man = ServerStatMan::new();
    man.update("mirror1.com", 10000, false);
    man.find_stat("mirror1.com").unwrap().increment_counter();
    man.update("mirror1.com", 10000, false);
    man.update("mirror2.com", 5000, true);
    man.find_stat("mirror2.com").unwrap().increment_counter();
    man.update("mirror2.com", 5000, true);
    man.update("mirror3.com", 2000, false);
    man.find_stat("mirror3.com").unwrap().increment_counter();
    man.update("mirror3.com", 2000, false);
    man.mark_failure("mirror3.com", 503);
    man.mark_failure("mirror3.com", 503);

    let temp_file = std::env::temp_dir().join("test_server_stat_roundtrip.json");
    let saved = man.save_to_file(&temp_file).unwrap();
    assert_eq!(saved, 3);

    let man2 = ServerStatMan::new();
    let loaded = man2.load_from_file(&temp_file).unwrap();
    assert_eq!(loaded, 3);

    let s1 = man2.find_stat("mirror1.com").unwrap();
    let s2 = man2.find_stat("mirror2.com").unwrap();
    let s3 = man2.find_stat("mirror3.com").unwrap();

    assert!(s1.get_single_avg_speed() > 0);
    assert!(s2.get_multi_avg_speed() > 0);
    assert_eq!(s3.get_consecutive_failures(), 2);
    assert_eq!(s3.get_last_error_code(), 503);

    let _ = std::fs::remove_file(&temp_file);
}

#[test]
fn test_load_nonexistent_file() {
    let man = ServerStatMan::new();
    let nonexistent = std::path::Path::new("/tmp/nonexistent_server_stat_12345.json");

    let loaded = man
        .load_from_file(nonexistent)
        .expect("Should return Ok(0) for nonexistent file");
    assert_eq!(loaded, 0);
    assert_eq!(man.count(), 0);
}

#[test]
fn test_save_empty_manager() {
    let man = ServerStatMan::new();
    let temp_file = std::env::temp_dir().join("test_server_stat_empty.json");

    let saved = man
        .save_to_file(&temp_file)
        .expect("Should save empty manager");
    assert_eq!(saved, 0);

    let content = std::fs::read_to_string(&temp_file).expect("Should read file");
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("Should be valid JSON");
    let servers = parsed.get("servers").unwrap().as_array().unwrap();
    assert!(servers.is_empty());

    let _ = std::fs::remove_file(&temp_file);
}

#[test]
fn test_load_merges_with_existing() {
    let man = ServerStatMan::new();
    man.update("existing.com", 1000, false);

    let temp_file = std::env::temp_dir().join("test_server_stat_merge.json");
    let man2 = ServerStatMan::new();
    man2.update("fromfile.com", 5000, false);
    let _ = man2.save_to_file(&temp_file);

    let loaded = man.load_from_file(&temp_file).unwrap();
    assert_eq!(loaded, 1);

    assert_eq!(man.count(), 2);
    assert!(man.find_stat("existing.com").is_some());
    assert!(man.find_stat("fromfile.com").is_some());

    let _ = std::fs::remove_file(&temp_file);
}

#[tokio::test]
async fn test_async_save_and_load() {
    let man = ServerStatMan::new();
    man.update("async.test.com", 8000, false);
    man.find_stat("async.test.com").unwrap().increment_counter();
    man.update("async.test.com", 8000, false);
    man.update("async2.test.com", 4000, true);
    man.find_stat("async2.test.com")
        .unwrap()
        .increment_counter();
    man.update("async2.test.com", 4000, true);

    let temp_file = std::env::temp_dir().join("test_server_stat_async.json");

    let saved = man
        .save_to_file_async(&temp_file)
        .await
        .expect("Async save should succeed");
    assert_eq!(saved, 2);

    let man2 = ServerStatMan::new();
    let loaded = man2
        .load_from_file_async(&temp_file)
        .await
        .expect("Async load should succeed");
    assert_eq!(loaded, 2);

    let stat = man2.find_stat("async.test.com").unwrap();
    assert!(stat.get_avg_speed() > 0);

    let _ = std::fs::remove_file(&temp_file);
}

#[test]
fn test_file_format_structure() {
    let man = ServerStatMan::new();
    man.update("format.test", 12345, false);

    let temp_file = std::env::temp_dir().join("test_server_stat_format.json");
    let _ = man.save_to_file(&temp_file);

    let content = std::fs::read_to_string(&temp_file).unwrap();
    let parsed: ServerStatFile = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed.version, "1.0");
    assert!(parsed.saved_at > 0);
    assert_eq!(parsed.servers.len(), 1);
    assert_eq!(parsed.servers[0].host, "format.test");
    assert_eq!(parsed.servers[0].download_speed, 12345);

    let _ = std::fs::remove_file(&temp_file);
}
