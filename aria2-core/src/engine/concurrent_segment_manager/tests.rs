use std::sync::Arc;

use super::mirror_selection::extract_host_from_url;
use super::types::SegmentStatus;
use super::ConcurrentSegmentManager;

#[test]
fn test_manager_creation_small_file() {
    let mgr = ConcurrentSegmentManager::new(1024, vec!["http://a.com/f".to_string()], None);
    assert_eq!(mgr.num_segments(), 1);
    assert_eq!(mgr.num_mirrors(), 1);
    assert_eq!(mgr.total_size(), 1024);
    assert!(!mgr.is_complete());
    assert!(mgr.has_pending_segments());
}

#[test]
fn test_manager_large_file_multi_segment() {
    let mgr = ConcurrentSegmentManager::new(
        3_000_000,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(1_000_000),
    );
    assert_eq!(mgr.num_segments(), 3);
    assert_eq!(mgr.num_mirrors(), 2);
}

#[test]
fn test_allocate_segments_round_robin() {
    let mut mgr = ConcurrentSegmentManager::new(
        3_000_000,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(1_000_000),
    );

    mgr.allocate_segments();

    let assigned_a: Vec<_> = mgr
        .segments
        .iter()
        .filter(|s| s.assigned_mirror == Some(0))
        .map(|s| s.index)
        .collect();
    let assigned_b: Vec<_> = mgr
        .segments
        .iter()
        .filter(|s| s.assigned_mirror == Some(1))
        .map(|s| s.index)
        .collect();

    assert!(!assigned_a.is_empty());
    assert!(!assigned_b.is_empty());
    assert_eq!(assigned_a.len() + assigned_b.len(), 3);
}

#[test]
fn test_complete_and_assemble() {
    let mut mgr =
        ConcurrentSegmentManager::new(200, vec!["http://x.com/f".to_string()], Some(100));

    mgr.allocate_segments();
    assert_eq!(mgr.progress(), 0.0);

    mgr.complete_segment(0, 100);
    assert!(!mgr.is_complete());
    assert!((mgr.progress() - 50.0).abs() < 0.01);

    mgr.complete_segment(1, 100);
    assert!(mgr.is_complete());
    assert!((mgr.progress() - 100.0).abs() < 0.01);

    assert_eq!(mgr.completed_bytes(), 200);
}

#[test]
fn test_fail_and_reassign() {
    let mut mgr = ConcurrentSegmentManager::new(
        200,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(100),
    );

    mgr.allocate_segments();

    let reassign = mgr.fail_segment(0);
    assert!(reassign.is_some());

    let seg = &mgr.segments[0];
    assert_eq!(seg.status, SegmentStatus::Pending);
    assert_eq!(seg.assigned_mirror, reassign);
    assert_eq!(seg.retry_count, 1);
}

#[test]
fn test_max_retries_exhausted() {
    let mut mgr =
        ConcurrentSegmentManager::new(100, vec!["http://a.com/f".to_string()], Some(100));
    mgr.set_max_retries(2);

    mgr.fail_segment(0);
    assert!(mgr.has_pending_segments());

    mgr.fail_segment(0);
    assert!(mgr.has_failed_segments());
    assert!(!mgr.has_pending_segments());
}

#[test]
fn test_empty_file() {
    let mgr = ConcurrentSegmentManager::new(0, vec!["http://x.com/f".to_string()], None);
    assert_eq!(mgr.num_segments(), 0);
    assert!(mgr.is_complete());
}

#[test]
fn test_next_pending_for_specific_mirror() {
    let mut mgr = ConcurrentSegmentManager::new(
        300,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(100),
    );

    let r = mgr.next_pending_segment_for_mirror(0);
    assert!(r.is_some());
    let (idx, off, len) = r.unwrap();
    assert_eq!(idx, 0);
    assert_eq!(off, 0);
    assert_eq!(len, 100);

    let r2 = mgr.next_pending_segment_for_mirror(1);
    assert!(r2.is_some());
    let (idx2, _, _) = r2.unwrap();
    assert_eq!(idx2, 1);
}

// ======================================================================
// Tests for Intelligent Mirror Selection
// ======================================================================

#[test]
fn test_new_with_selector() {
    use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;
    use crate::selector::server_stat_man::ServerStatMan;

    let stat_man = Arc::new(ServerStatMan::new());
    let urls = vec![
        "http://mirror1.com/file".to_string(),
        "http://mirror2.com/file".to_string(),
    ];
    let selector = Box::new(AdaptiveUriSelector::new_with_uris(
        Arc::clone(&stat_man),
        urls.clone(),
    ));

    let mgr = ConcurrentSegmentManager::new_with_selector(
        1_000_000,
        urls,
        Some(500_000),
        stat_man,
        selector,
    );

    assert_eq!(mgr.num_segments(), 2);
    assert_eq!(mgr.num_mirrors(), 2);
    assert!(mgr.has_intelligent_selection());
}

#[test]
fn test_select_mirror_for_next_segment_without_selector() {
    let mut mgr = ConcurrentSegmentManager::new(
        300,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(100),
    );

    // Without UriSelector, should use fallback
    let result = mgr.select_mirror_for_next_segment();
    assert!(result.is_some());

    let (mirror_idx, (seg_idx, offset, len)) = result.unwrap();
    assert_eq!(seg_idx, 0);
    assert_eq!(offset, 0);
    assert_eq!(len, 100);
    assert!(mirror_idx < 2);
}

#[test]
fn test_select_mirror_for_next_segment_with_selector() {
    use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;
    use crate::selector::server_stat_man::ServerStatMan;

    let stat_man = Arc::new(ServerStatMan::new());
    let urls = vec![
        "http://fast.com/f".to_string(),
        "http://slow.com/f".to_string(),
    ];

    // Make fast.com have better stats
    stat_man.update("fast.com", 1_000_000, false);
    stat_man.update("slow.com", 1000, false);
    let fast_stat = stat_man.find_stat("fast.com").unwrap();
    fast_stat.increment_counter();
    let slow_stat = stat_man.find_stat("slow.com").unwrap();
    slow_stat.increment_counter();

    let selector = Box::new(AdaptiveUriSelector::new_with_uris(
        Arc::clone(&stat_man),
        urls.clone(),
    ));

    let mut mgr =
        ConcurrentSegmentManager::new_with_selector(300, urls, Some(100), stat_man, selector);

    let result = mgr.select_mirror_for_next_segment();
    assert!(result.is_some());

    let (mirror_idx, _) = result.unwrap();
    // Fast mirror (index 0) should be selected
    assert_eq!(mirror_idx, 0);
}

#[test]
fn test_report_segment_complete_updates_stats() {
    use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;
    use crate::selector::server_stat_man::ServerStatMan;

    let stat_man = Arc::new(ServerStatMan::new());
    let urls = vec!["http://test.mirror.com/f".to_string()];

    let selector = Box::new(AdaptiveUriSelector::new_with_uris(
        Arc::clone(&stat_man),
        urls.clone(),
    ));

    let mut mgr = ConcurrentSegmentManager::new_with_selector(
        100,
        urls,
        Some(100),
        stat_man.clone(),
        selector,
    );

    mgr.allocate_segments();

    // Report completion with 1 MB/s speed
    let success = mgr.report_segment_complete(0, 100, 1_000_000, false);
    assert!(success);

    // Check that stats were updated
    let stat = stat_man.find_stat("test.mirror.com").unwrap();
    assert!(stat.get_download_speed() > 0);
}

#[test]
fn test_report_segment_failed_updates_stats() {
    use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;
    use crate::selector::server_stat_man::ServerStatMan;

    let stat_man = Arc::new(ServerStatMan::new());
    let urls = vec![
        "http://failing.mirror.com/f".to_string(),
        "http://backup.mirror.com/f".to_string(),
    ];

    let selector = Box::new(AdaptiveUriSelector::new_with_uris(
        Arc::clone(&stat_man),
        urls.clone(),
    ));

    let mut mgr = ConcurrentSegmentManager::new_with_selector(
        100,
        urls,
        Some(100),
        stat_man.clone(),
        selector,
    );

    mgr.allocate_segments();

    // Report failure
    let reassign = mgr.report_segment_failed(0, 503);
    assert!(reassign.is_some());

    // Check that stats were updated
    let stat = stat_man.find_stat("failing.mirror.com").unwrap();
    assert_eq!(stat.get_consecutive_failures(), 1);
    assert_eq!(stat.get_last_error_code(), 503);
}

#[test]
fn test_extract_host_from_url() {
    assert_eq!(
        extract_host_from_url("http://example.com/path"),
        "example.com"
    );
    assert_eq!(
        extract_host_from_url("https://host:8080/file?q=1"),
        "host:8080"
    );
    assert_eq!(extract_host_from_url("ftp://server.com"), "server.com");
    assert_eq!(extract_host_from_url("not-a-url"), "not-a-url");
}

#[test]
fn test_get_mirror_url() {
    let mgr = ConcurrentSegmentManager::new(
        100,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(100),
    );

    assert_eq!(mgr.get_mirror_url(0), Some("http://a.com/f"));
    assert_eq!(mgr.get_mirror_url(1), Some("http://b.com/f"));
    assert_eq!(mgr.get_mirror_url(999), None);
}

#[test]
fn test_mirror_active_segments() {
    let mut mgr =
        ConcurrentSegmentManager::new(300, vec!["http://a.com/f".to_string()], Some(100));

    assert_eq!(mgr.mirror_active_segments(0), 0);
    assert_eq!(mgr.num_segments(), 3);

    // Set max connections to allow all 3 segments
    mgr.set_max_connections_per_mirror(3);

    mgr.allocate_segments();
    // After allocation, all 3 segments should be assigned to the single mirror
    assert_eq!(mgr.mirror_active_segments(0), 3);
}

#[test]
fn test_no_intelligent_selection_by_default() {
    let mgr = ConcurrentSegmentManager::new(100, vec!["http://a.com/f".to_string()], Some(100));

    assert!(!mgr.has_intelligent_selection());
}

// ======================================================================
// Tests for atomic / lock-free segment allocation (Phase E1)
// ======================================================================

/// Verify that `allocate_next_index` is lock-free and never issues a
/// duplicate or missing index when hammered from many threads.
///
/// 16 threads each call `allocate_next_index` 1000 times against a shared
/// `Arc<ConcurrentSegmentManager>` (16000 segments of 1 byte each). Because
/// `allocate_next_index` takes only `&self` and uses `fetch_add`, every call
/// must receive a distinct index in `0..16000` with no duplicates and no gaps.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn test_segment_allocation_is_lock_free() {
    use std::collections::HashSet;

    // 16000 segments of size 1 byte = 16000 segments.
    let manager = Arc::new(ConcurrentSegmentManager::new(
        16000,
        vec!["http://test".into()],
        Some(1),
    ));
    assert_eq!(manager.num_segments(), 16000);

    let collected: Arc<tokio::sync::Mutex<Vec<u32>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let mut handles = Vec::with_capacity(16);
    for _ in 0..16 {
        let m = manager.clone();
        let c = collected.clone();
        handles.push(tokio::spawn(async move {
            // Collect locally first to minimize lock contention on `collected`.
            let mut local = Vec::with_capacity(1000);
            for _ in 0..1000 {
                if let Some(idx) = m.allocate_next_index() {
                    local.push(idx);
                }
            }
            c.lock().await.extend(local);
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let indices = collected.lock().await.clone();
    assert_eq!(
        indices.len(),
        16000,
        "should have allocated all 16000 segments"
    );

    // Verify no duplicates.
    let set: HashSet<u32> = indices.iter().copied().collect();
    assert_eq!(
        set.len(),
        16000,
        "all indices must be unique (no duplicates)"
    );

    // Verify all indices 0..16000 are present.
    for i in 0..16000u32 {
        assert!(set.contains(&i), "missing index {}", i);
    }
}

/// Verify the allocation hint advances indices in order and that
/// `reset_allocation_index` rewinds the scan start position.
///
/// The hint optimization must (a) return segments in ascending index order
/// without re-scanning from 0 each call, (b) use wraparound to find a
/// Pending segment that lies behind the current hint, and (c) be rewound
/// to 0 by `reset_allocation_index`.
#[test]
fn test_allocation_hint_advances_and_resets() {
    let mut mgr =
        ConcurrentSegmentManager::new(500, vec!["http://a.com/f".to_string()], Some(100));
    // 5 segments; let the single mirror accept all of them.
    mgr.set_max_connections_per_mirror(10);
    assert_eq!(mgr.num_segments(), 5);

    // Claim segments one at a time. The hint advances so each allocation
    // starts scanning right after the last claim, yielding indices 0..5
    // in order without re-scanning already-assigned segments.
    let mut claimed = Vec::new();
    while let Some((idx, _, _)) = mgr.next_pending_segment_for_mirror(0) {
        claimed.push(idx);
    }
    assert_eq!(claimed, vec![0, 1, 2, 3, 4]);

    // All segments are Downloading; no pending segment remains.
    assert!(mgr.next_pending_segment_for_mirror(0).is_none());

    // Simulate a retry: re-mark segment 1 as Pending. The hint currently
    // points past the end (5 -> wraps to 0), so the wraparound scan must
    // visit index 1 to find it.
    mgr.segments[1].status = SegmentStatus::Pending;
    let next = mgr.next_pending_segment_for_mirror(0);
    assert!(next.is_some());
    assert_eq!(next.unwrap().0, 1);

    // reset_allocation_index rewinds the hint to 0 without touching statuses.
    mgr.reset_allocation_index();
    // Mark segment 3 as Pending; with the hint rewound to 0 the scan walks
    // forward from index 0 and finds segment 3 (the only Pending one).
    mgr.segments[3].status = SegmentStatus::Pending;
    let next = mgr.next_pending_segment_for_mirror(0);
    assert!(next.is_some());
    assert_eq!(next.unwrap().0, 3);
}
