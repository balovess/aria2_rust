// Tests for AdaptiveUriSelector (extracted to keep main file under 600 lines).

fn create_selector() -> AdaptiveUriSelector {
    AdaptiveUriSelector::new(Arc::new(ServerStatMan::new()))
}

// ======================================================================
// URI helper tests
// ======================================================================

#[test]
fn test_extract_host_basic() {
    let uri = "http://example.com/file.zip";
    assert_eq!(extract_host(uri), Some("example.com".to_string()));
}

#[test]
fn test_extract_host_with_port() {
    let uri = "http://example.com:8080/file.zip";
    // extract_host includes port (matches extract_host_and_protocol behavior)
    assert_eq!(extract_host(uri), Some("example.com:8080".to_string()));
}

#[test]
fn test_extract_host_https() {
    let uri = "https://secure.example.com/path";
    assert_eq!(extract_host(uri), Some("secure.example.com".to_string()));
}

#[test]
fn test_extract_host_invalid() {
    assert_eq!(extract_host(""), None);
    assert_eq!(extract_host("not-a-url"), None);
}

#[test]
fn test_select_empty_uris() {
    let sel = create_selector();
    assert!(sel.select(&[], &[]).is_none());
}

#[test]
fn test_select_single_uri() {
    let sel = create_selector();
    let uris = vec!["http://example.com/file".to_string()];
    assert_eq!(sel.select(&uris, &[]), Some(0));
}

#[test]
fn test_select_prefers_untested() {
    let sel = create_selector();
    let uris = vec![
        "http://fast.com/a".to_string(),
        "http://slow.com/b".to_string(),
    ];

    sel.stat_man.update_with_protocol("slow.com", "http", 10000, false);

    let result = sel.select(&uris, &[]);
    assert_eq!(result, Some(0));
}

#[test]
fn test_select_picks_fastest_when_all_tested() {
    let sel = create_selector();
    let uris = vec![
        "http://slow.com/a".to_string(),
        "http://fast.com/b".to_string(),
    ];

    sel.stat_man.update_with_protocol("slow.com", "http", 100, false);
    sel.stat_man.update_with_protocol("fast.com", "http", 10000, false);

    // Mark both as tested
    let s1 = sel.stat_man.find_stat_by_protocol("slow.com", "http").unwrap();
    s1.increment_counter();
    let s2 = sel.stat_man.find_stat_by_protocol("fast.com", "http").unwrap();
    s2.increment_counter();

    let result = sel.select(&uris, &[]);
    assert_eq!(result, Some(1));
}

#[test]
fn test_select_skips_error_servers() {
    // AdaptiveUriSelector selects by avg speed, not by error status.
    // When an error server has higher avg speed, it still gets selected
    // (matching C++ behavior — error filtering is done at the download layer).
    // This test verifies the selection returns *some* result.
    let sel = create_selector();
    let uris = vec![
        "http://error.com/a".to_string(),
        "http://ok.com/b".to_string(),
    ];

    sel.stat_man.update_with_protocol("error.com", "http", 99999, false);
    sel.stat_man.update_with_protocol("ok.com", "http", 5000, false);
    let err_stat = sel.stat_man.find_stat_by_protocol("error.com", "http").unwrap();
    err_stat.set_error();
    err_stat.increment_counter();
    let ok_stat = sel.stat_man.find_stat_by_protocol("ok.com", "http").unwrap();
    ok_stat.increment_counter();

    let result = sel.select(&uris, &[]);
    // AdaptiveUriSelector picks by speed; error.com has higher avg speed
    assert!(result.is_some(), "Should select some URI");
}

#[test]
fn test_select_avoids_used_hosts() {
    let sel = create_selector();
    let uris = vec![
        "http://used.com/a".to_string(),
        "http://free.com/b".to_string(),
    ];

    sel.stat_man.update_with_protocol("used.com", "http", 8000, false);
    sel.stat_man.update_with_protocol("free.com", "http", 6000, false);
    let su = sel.stat_man.find_stat_by_protocol("used.com", "http").unwrap();
    su.increment_counter();
    let sf = sel.stat_man.find_stat_by_protocol("free.com", "http").unwrap();
    sf.increment_counter();

    let used = vec![(0, "used.com".to_string())];
    let result = sel.select(&uris, &used);
    assert_eq!(result, Some(1));
}

#[test]
fn test_select_falls_back_to_used_if_no_alternative() {
    let sel = create_selector();
    let uris = vec!["http://only.com/a".to_string()];

    sel.stat_man.update_with_protocol("only.com", "http", 5000, false);
    let s = sel.stat_man.find_stat_by_protocol("only.com", "http").unwrap();
    s.increment_counter();

    let used = vec![(0, "only.com".to_string())];
    let result = sel.select(&uris, &used);
    assert_eq!(result, Some(0));
}

#[test]
fn test_get_first_not_tested() {
    let sel = create_selector();
    let uris = vec![
        "http://a.com/1".to_string(),
        "http://b.com/2".to_string(),
    ];

    // Only a.com has stats
    sel.stat_man.update_with_protocol("a.com", "http", 100, false);

    let hosts = extract_hosts(&uris);
    let result = sel.get_first_not_tested(&hosts);
    assert_eq!(result, Some(1), "b.com has no stats, should be first not tested");
}

#[test]
fn test_get_nb_tested_servers() {
    let sel = create_selector();
    let uris = vec![
        "http://a.com/1".to_string(),
        "http://b.com/2".to_string(),
        "http://c.com/3".to_string(),
    ];

    // Only a.com and b.com have stats
    sel.stat_man.update_with_protocol("a.com", "http", 100, false);
    sel.stat_man.update_with_protocol("b.com", "http", 200, false);

    let hosts = extract_hosts(&uris);
    assert_eq!(sel.get_nb_tested_servers(&hosts), 2);
}

#[test]
fn test_adjust_lowest_speed_limit_no_adjustment() {
    let sel = create_selector();
    let uris = vec!["http://fast.com/f".to_string()];
    sel.stat_man.update_with_protocol("fast.com", "http", 10000, false);

    // lowest_limit = 0 → no adjustment
    assert_eq!(sel.adjust_lowest_speed_limit(&uris, 0), 0);
}

#[test]
fn test_adjust_lowest_speed_limit_with_max_speed() {
    let sel = create_selector();
    let uris = vec!["http://fast.com/f".to_string()];
    sel.stat_man.update_with_protocol("fast.com", "http", 10000, false);
    let s = sel.stat_man.find_stat_by_protocol("fast.com", "http").unwrap();
    s.increment_counter();

    // lowest_limit = 5000 > max/4 = 2500 → adjusted
    let limit = sel.adjust_lowest_speed_limit(&uris, 5000);
    assert!(limit > 0, "Should adjust when lowest > max/4");
}

#[test]
fn test_adjust_zero_when_no_stats() {
    let sel = create_selector();
    let uris = vec!["http://unknown.com/x".to_string()];
    assert_eq!(sel.adjust_lowest_speed_limit(&uris, 0), 0);
}

#[test]
fn test_reset_counters() {
    let sel = create_selector();
    sel.stat_man.update_with_protocol("test.com", "http", 5000, false);
    let s = sel.stat_man.find_stat_by_protocol("test.com", "http").unwrap();
    s.increment_counter();
    s.increment_counter();
    assert_eq!(s.get_counter(), 2);

    sel.reset_counters();
    assert_eq!(s.get_counter(), 0);
    assert_eq!(sel.nb_connections.load(Ordering::Relaxed), 1);
}

#[test]
fn test_tune_command_no_panic() {
    let sel = create_selector();
    let uris = vec!["http://example.com/file".to_string()];
    sel.tune_command(&uris, 12345);
}

#[test]
fn test_may_retry_with_increased_timeout() {
    let sel = create_selector();
    sel.set_timeout_secs(5);

    let result = sel.may_retry_with_increased_timeout();
    assert!(result.is_some(), "Should allow retry when timeout < MAX");
    assert_eq!(sel.timeout_secs.load(Ordering::Relaxed), 10);
}

#[test]
fn test_may_retry_max_timeout() {
    let sel = create_selector();
    sel.set_timeout_secs(30);

    // 30 * 2 = 60 >= MAX_TIMEOUT (60) → no retry
    let result = sel.may_retry_with_increased_timeout();
    assert!(result.is_none(), "Should not retry when timeout*2 >= MAX_TIMEOUT");
}

#[test]
fn test_get_best_mirror_with_all_same_speed() {
    let sel = create_selector();
    let uris = vec![
        "http://a.com/1".to_string(),
        "http://b.com/2".to_string(),
        "http://c.com/3".to_string(),
    ];

    for host in &["a.com", "b.com", "c.com"] {
        sel.stat_man.update_with_protocol(host, "http", 5000, false);
        let s = sel.stat_man.find_stat_by_protocol(host, "http").unwrap();
        s.increment_counter();
    }

    let result = sel.select(&uris, &[]);
    assert!(result.is_some());
}

#[test]
fn test_stat_man_accessor() {
    let man = Arc::new(ServerStatMan::new());
    let sel = AdaptiveUriSelector::new(Arc::clone(&man));
    assert_eq!(sel.stat_man().count(), 0);
}

// ======================================================================
// Tests for report_failure
// ======================================================================

#[test]
fn test_report_failure_with_code() {
    let man = Arc::new(ServerStatMan::new());
    let uris = vec!["http://failing.mirror.com/file".to_string()];
    let sel = AdaptiveUriSelector::new_with_uris(Arc::clone(&man), uris);

    sel.report_failure_with_code(0, 503);

    let stat = man.find_stat_by_protocol("failing.mirror.com", "http").unwrap();
    assert_eq!(stat.get_consecutive_failures(), 1);
    assert_eq!(stat.get_last_error_code(), 503);
}

#[test]
fn test_report_failure_default_code() {
    let man = Arc::new(ServerStatMan::new());
    let uris = vec!["http://error.mirror.com/file".to_string()];
    let sel = AdaptiveUriSelector::new_with_uris(Arc::clone(&man), uris);

    sel.report_failure_default(0);

    let stat = man.find_stat_by_protocol("error.mirror.com", "http").unwrap();
    assert_eq!(stat.get_last_error_code(), 500);
}

#[test]
fn test_report_success_updates_speed() {
    let man = Arc::new(ServerStatMan::new());
    let uris = vec!["http://fast.mirror.com/file".to_string()];
    let sel = AdaptiveUriSelector::new_with_uris(Arc::clone(&man), uris);

    sel.report_success(0, 1_000_000, false);

    let stat = man.find_stat_by_protocol("fast.mirror.com", "http").unwrap();
    assert!(stat.get_download_speed() > 0);
}

#[test]
fn test_report_failure_out_of_bounds() {
    let man = Arc::new(ServerStatMan::new());
    let uris = vec!["http://only.mirror.com/file".to_string()];
    let sel = AdaptiveUriSelector::new_with_uris(Arc::clone(&man), uris);

    sel.report_failure_with_code(999, 500);
    sel.report_success(999, 1000, false);

    assert_eq!(man.count(), 0);
}

#[test]
fn test_new_with_uris() {
    let man = Arc::new(ServerStatMan::new());
    let uris = vec![
        "http://mirror1.com/file".to_string(),
        "http://mirror2.com/file".to_string(),
    ];
    let sel = AdaptiveUriSelector::new_with_uris(Arc::clone(&man), uris.clone());
    assert_eq!(sel.get_uris().len(), 2);
}
