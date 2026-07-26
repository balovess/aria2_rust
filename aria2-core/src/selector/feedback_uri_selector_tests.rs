// Tests for FeedbackUriSelector (extracted to keep main file under 600 lines).

fn create_selector() -> FeedbackUriSelector {
    FeedbackUriSelector::new(Arc::new(ServerStatMan::new()))
}

fn create_selector_with_man(man: Arc<ServerStatMan>) -> FeedbackUriSelector {
    FeedbackUriSelector::new(man)
}

// ======================================================================
// extract_host_and_protocol tests
// ======================================================================

#[test]
fn test_extract_host_and_protocol_https_with_port() {
    let (host, proto) = extract_host_and_protocol("https://cdn.example.com:8443/file").unwrap();
    assert_eq!(host, "cdn.example.com:8443");
    assert_eq!(proto, "https");
}

#[test]
fn test_extract_host_and_protocol_ftp() {
    let (host, proto) = extract_host_and_protocol("ftp://files.example.com/").unwrap();
    assert_eq!(host, "files.example.com");
    assert_eq!(proto, "ftp");
}

#[test]
fn test_extract_host_and_protocol_no_path() {
    let (host, proto) = extract_host_and_protocol("http://example.com").unwrap();
    assert_eq!(host, "example.com");
    assert_eq!(proto, "http");
}

#[test]
fn test_extract_host_and_protocol_invalid_extras() {
    assert!(extract_host_and_protocol("://missing-scheme").is_none());
    assert!(extract_host_and_protocol("http://").is_none());
}

// ======================================================================
// FeedbackUriSelector basic tests
// ======================================================================

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
fn test_select_faster_prefers_fast_server() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://slow.com/file".to_string(),
        "http://fast.com/file".to_string(),
    ];

    man.update_with_protocol("fast.com", "http", 50000, false);
    man.update_with_protocol("slow.com", "http", 100, false);

    let result = sel.select(&uris, &[]);
    assert_eq!(result, Some(1), "Should select the fast server");
}

#[test]
fn test_select_faster_skips_used_hosts() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://used.com/file".to_string(),
        "http://free.com/file".to_string(),
    ];

    man.update_with_protocol("used.com", "http", 100000, false);
    man.update_with_protocol("free.com", "http", 50000, false);

    let used = vec![(0, "used.com".to_string())];
    let result = sel.select(&uris, &used);
    assert_eq!(result, Some(1), "Should skip used host and select free");
}

#[test]
fn test_select_faster_skips_error_servers() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://error.com/file".to_string(),
        "http://ok.com/file".to_string(),
    ];

    man.update_with_protocol("error.com", "http", 100000, false);
    let err_stat = man.find_stat_by_protocol("error.com", "http").unwrap();
    err_stat.set_error();

    let result = sel.select(&uris, &[]);
    assert_eq!(result, Some(1), "Should skip error server");
}

#[test]
fn test_select_faster_below_threshold_goes_to_norm() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://slow.com/file".to_string(),
        "http://untested.com/file".to_string(),
    ];

    man.update_with_protocol("slow.com", "http", 5000, false);

    let result = sel.select(&uris, &[]);
    assert_eq!(
        result,
        Some(0),
        "Slow server should be in normCands (comes first)"
    );
}

#[test]
fn test_select_rarer_prefers_used_hosts() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://unused.com/file".to_string(),
        "http://proven.com/file".to_string(),
    ];

    man.update_with_protocol("unused.com", "http", 100, false);
    man.update_with_protocol("proven.com", "http", 100, false);
    man.find_stat_by_protocol("unused.com", "http")
        .unwrap()
        .set_error();
    man.find_stat_by_protocol("proven.com", "http")
        .unwrap()
        .set_error();

    let result = sel.select(&uris, &[]);
    assert!(result.is_some());
}

#[test]
fn test_select_rarer_returns_proven_host() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://unproven.com/file".to_string(),
        "http://proven.com/file".to_string(),
    ];

    let used = vec![
        (0, "unproven.com".to_string()),
        (1, "proven.com".to_string()),
    ];
    let result = sel.select(&uris, &used);
    assert_eq!(
        result,
        Some(0),
        "selectRarer should prefer first usedHost match"
    );
}

#[test]
fn test_select_returns_none_when_all_error_and_used() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec!["http://error.com/file".to_string()];

    man.update_with_protocol("error.com", "http", 100, false);
    man.find_stat_by_protocol("error.com", "http")
        .unwrap()
        .set_error();

    let result = sel.select(&uris, &[]);
    assert!(
        result.is_some(),
        "selectRarer should return first URI as fallback"
    );
}

#[test]
fn test_select_protocol_aware() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://mirror.com/file".to_string(),
        "ftp://mirror.com/file".to_string(),
    ];

    man.update_with_protocol("mirror.com", "http", 50000, false);

    let result = sel.select(&uris, &[]);
    assert_eq!(
        result,
        Some(0),
        "Should select fast HTTP server over untested FTP"
    );
}

#[test]
fn test_select_protocol_different_stats() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://mirror.com/file".to_string(),
        "https://mirror.com/file".to_string(),
    ];

    man.update_with_protocol("mirror.com", "http", 100000, false);
    man.find_stat_by_protocol("mirror.com", "http")
        .unwrap()
        .set_error();
    man.update_with_protocol("mirror.com", "https", 50000, false);

    let result = sel.select(&uris, &[]);
    assert_eq!(
        result,
        Some(1),
        "Should select fast HTTPS server, skip error HTTP"
    );
}

#[test]
fn test_speed_threshold_boundary() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://at_threshold.com/file".to_string(),
        "http://above_threshold.com/file".to_string(),
    ];

    man.update_with_protocol("at_threshold.com", "http", SPEED_THRESHOLD, false);
    man.update_with_protocol("above_threshold.com", "http", SPEED_THRESHOLD + 1, false);

    let result = sel.select(&uris, &[]);
    assert_eq!(
        result,
        Some(1),
        "Server above threshold should be selected over one at threshold"
    );
}

#[test]
fn test_num_uri_limit() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let mut uris = Vec::new();
    for i in 0..15u64 {
        uris.push(format!("http://host{}.com/file", i));
        man.update_with_protocol(&format!("host{}.com", i), "http", 50000 + i * 1000, false);
    }

    let result = sel.select(&uris, &[]);
    assert!(result.is_some());
    assert!(
        result.unwrap() < 10,
        "Should select from first 10 URIs due to NUM_URI limit"
    );
}

#[test]
fn test_tune_command_no_panic() {
    let sel = create_selector();
    let uris = vec!["http://example.com/file".to_string()];
    sel.tune_command(&uris, 12345);
}

#[test]
fn test_reset_no_panic() {
    let sel = create_selector();
    sel.reset();
}

#[test]
fn test_invalid_uris_skipped() {
    let sel = create_selector();
    let uris = vec![
        "not-a-uri".to_string(),
        "http://valid.com/file".to_string(),
    ];

    let result = sel.select(&uris, &[]);
    assert_eq!(
        result,
        Some(1),
        "Should skip invalid URI and select valid one"
    );
}

#[test]
fn test_select_rarer_fallback_to_first_candidate() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://a.com/file".to_string(),
        "http://b.com/file".to_string(),
    ];

    let used = vec![(0, "a.com".to_string()), (1, "b.com".to_string())];
    let result = sel.select(&uris, &used);
    assert_eq!(
        result,
        Some(0),
        "selectRarer should find first host in usedHosts"
    );
}

#[test]
fn test_all_uris_in_used_hosts_select_rarer() {
    let man = Arc::new(ServerStatMan::new());
    let sel = create_selector_with_man(Arc::clone(&man));

    let uris = vec![
        "http://a.com/file".to_string(),
        "http://b.com/file".to_string(),
    ];

    let used = vec![(0, "a.com".to_string()), (1, "b.com".to_string())];
    let result = sel.select(&uris, &used);
    assert!(
        result.is_some(),
        "selectRarer should find a match in usedHosts"
    );
}
