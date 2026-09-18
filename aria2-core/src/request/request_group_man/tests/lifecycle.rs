use super::*;

/// A group paused by `reduce_to_limit()` carries the restart flag; when
/// it is re-queued the flag must be consumed so the group auto-resumes
/// (C++ `releaseRuntimeResource()` clears the pause request).
#[test]
fn test_restart_requested_group_auto_resumes_on_requeue() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    man.fill_from_reserver();

    // Simulate reduce_to_limit(): pause + restart request.
    {
        let group = man.find_group(gid).unwrap();
        let mut g = group.recover_mut();
        g.pause().unwrap();
        g.request_restart();
    }

    let requeued = man.requeue_non_terminal_groups(None);
    assert_eq!(requeued, 1);

    // The restart flag was consumed and the group is Waiting again.
    {
        let group = man.find_group(gid).unwrap();
        let g = group.recover();
        assert_eq!(g.status(), DownloadStatus::Waiting);
        assert!(!g.is_restart_requested(), "restart flag must be consumed");
        assert!(
            !g.is_pause_requested(),
            "restart consumption must clear the pause request"
        );
    }

    // Promotion picks it up immediately (slot permitting).
    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1);
    assert_eq!(man.active_count(), 1);
}

#[test]
fn test_active_remove_requests_halt_without_removing_group() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    man.fill_from_reserver();

    man.remove_group(gid).unwrap();

    let group = man.find_group(gid).expect("active group must be retained");
    let guard = group.recover();
    assert!(guard.is_halt_requested());
    assert_eq!(guard.get_halt_reason(), HaltReason::UserRequest);
    assert_eq!(man.stopped_count(), 0);
}

#[test]
fn test_force_remove_requests_force_halt_without_removing_group() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    man.fill_from_reserver();

    man.force_remove_group(gid).unwrap();

    let group = man.find_group(gid).expect("active group must be retained");
    let guard = group.recover();
    assert!(guard.is_force_halt_requested());
    assert_eq!(guard.get_halt_reason(), HaltReason::UserRequest);
}

#[test]
fn test_force_remove_waits_for_lifecycle_transition_lock() {
    let man = Arc::new(RequestGroupMan::new());
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    man.fill_from_reserver();

    let lifecycle = man.lifecycle_lock.lock().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let worker = Arc::clone(&man);
    let handle = thread::spawn(move || {
        started_tx.send(()).unwrap();
        completed_tx.send(worker.force_remove_group(gid)).unwrap();
    });

    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(
        completed_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "force removal must wait for the lifecycle transition lock"
    );

    drop(lifecycle);
    assert!(
        completed_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    handle.join().unwrap();

    let group = man
        .find_group(gid)
        .expect("active group must remain indexed");
    assert!(group.recover().is_force_halt_requested());
}

// ── Remove writes a REMOVED stopped result ──────────────────────────

/// `aria2.remove` must record a REMOVED DownloadResult in the stopped
/// storage so `tellStopped` / `getDownloadResult` can surface it.

#[test]
fn test_remove_group_writes_stopped_removed_result() {
    use crate::request::request_group::DownloadResultCode;

    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

    man.remove_group(gid).unwrap();

    assert!(man.find_group(gid).is_none(), "group must be removed");
    assert_eq!(man.stopped_count(), 1, "REMOVED result must be stored");
    let result = man
        .find_stopped_result(&gid.to_hex_string())
        .expect("stopped result must be findable by GID");
    assert_eq!(result.status, DownloadStatus::Removed);
    assert_eq!(result.code, DownloadResultCode::Removed);
}

// ── Spawn failure must not leave a zombie in active ─────────────────

/// A group whose download task failed to spawn must be removed from the
/// active list and recorded as an error instead of staying there forever.

#[test]
fn test_fail_spawned_group_removes_from_active_and_records_error() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    man.fill_from_reserver();
    assert_eq!(man.active.len(), 1);

    let ok = man.fail_spawned_group(gid, "Failed to spawn download task");
    assert!(ok, "failed-spawn group should be handled");
    assert!(
        man.find_group(gid).is_none(),
        "group must leave the manager"
    );
    assert_eq!(man.active.len(), 0, "group must not stay in active");

    let result = man
        .find_stopped_result(&gid.to_hex_string())
        .expect("failed-spawn group must have a stopped result");
    assert!(
        matches!(result.status, DownloadStatus::Error(_)),
        "failed-spawn group must be recorded as an error, got {:?}",
        result.status
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn batch_pause_operations_cover_both_metalink_graph_groups() {
    let man = RequestGroupMan::new();
    let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
        "https://example.test/file.torrent",
        "file.bin",
        &DownloadOptions::default(),
        GroupId::new(81),
        GroupId::new(82),
    )
    .unwrap();
    let (metadata_gid, payload_gid) = man.add_metalink_graph(graph).unwrap();

    man.pause_all();
    for gid in [metadata_gid, payload_gid] {
        let group = man.find_group(gid).unwrap();
        assert!(group.recover().status().is_paused());
    }

    man.unpause_all();
    for gid in [metadata_gid, payload_gid] {
        let group = man.find_group(gid).unwrap();
        assert_eq!(group.recover().status(), DownloadStatus::Waiting);
    }

    man.force_pause_all();
    for gid in [metadata_gid, payload_gid] {
        let group = man.find_group(gid).unwrap();
        let group = group.recover();
        assert!(group.status().is_paused());
        assert!(group.is_force_pause_requested());
    }
}
