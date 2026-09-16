use super::*;

#[test]
fn test_add_group_with_gid_rejects_duplicate_gid() {
    let man = RequestGroupMan::new();
    let gid = GroupId::new(42);
    let options = DownloadOptions::default();
    man.add_group_with_gid(
        gid,
        vec!["http://example.com/file.bin".to_string()],
        options.clone(),
    )
    .unwrap();

    let result = man.add_group_with_gid(
        gid,
        vec!["http://example.com/other.bin".to_string()],
        options,
    );
    assert!(result.is_err());
    assert_eq!(man.count(), 1);
}

/// Test that groups go to reserved queue by default.

#[test]
fn test_add_group_goes_to_reserved() {
    let man = RequestGroupMan::new();
    let _gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    assert_eq!(man.active.len(), 0, "No groups should be active yet");
    assert_eq!(man.reserved.len(), 1, "Group should be in reserved queue");
    assert_eq!(man.count(), 1);
}

#[test]
fn test_seed_only_groups_do_not_consume_active_limit() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["magnet:?xt=urn:btih:test".to_string()],
            DownloadOptions {
                bt_detach_seed_only: true,
                ..DownloadOptions::default()
            },
        )
        .unwrap();
    man.fill_from_reserver();
    let group = man.find_group(gid).unwrap();
    group.recover().enable_seed_only();
    assert_eq!(man.active_count(), 0);
}

/// Test max_concurrent default and setting.

#[test]
fn test_max_concurrent() {
    let man = RequestGroupMan::new();
    assert_eq!(man.max_concurrent(), 5); // default
    man.set_max_concurrent(10);
    assert_eq!(man.max_concurrent(), 10);
    man.set_max_concurrent(0); // unlimited
    assert_eq!(man.max_concurrent(), 0);
}

#[test]
fn blocked_reserved_group_does_not_starve_later_runnable_group() {
    let man = RequestGroupMan::new();
    man.set_max_concurrent(1);

    let blocked = man
        .add_group(
            vec!["http://example.com/blocked.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    man.find_group(blocked)
        .expect("blocked group should be registered")
        .recover_mut()
        .pause()
        .unwrap();

    let runnable = man
        .add_group(
            vec!["http://example.com/runnable.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].recover().gid(), runnable);
    assert!(
        man.find_group(blocked)
            .unwrap()
            .recover()
            .status()
            .is_paused()
    );
    assert_eq!(man.reserved.len(), 1);
}

/// Test find_group searches both active and reserved.

#[test]
fn test_find_group_searches_both() {
    let man = RequestGroupMan::new();
    let gid1 = man
        .add_group(vec!["http://a.com".to_string()], DownloadOptions::default())
        .unwrap();

    // Manually add a group to active (normally done by promotion).
    let gid2 = GroupId(999);
    let group2 = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid2,
        vec!["http://b.com".to_string()],
        DownloadOptions::default(),
    )));
    // Set it as Active
    group2.recover_mut().start().unwrap();
    assert!(man.register_group(Arc::clone(&group2)));
    man.active.insert(gid2, group2);

    // Should find gid1 in reserved.
    assert!(man.find_group(gid1).is_some());
    // Should find gid2 in active.
    assert!(man.find_group(gid2).is_some());
    // Should not find nonexistent.
    assert!(man.find_group(GroupId(12345)).is_none());
}

#[test]
fn test_find_group_stays_visible_during_queue_transfer() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1);
    let canonical = man.find_group(gid).expect("group must be indexed");

    // Exercise the two storage mutations separately. The canonical index
    // must keep the GID visible in the interval between them.
    let moved = man.active.remove(&gid).expect("group must be active").1;
    let during_active_removal = man
        .find_group(gid)
        .expect("lookup must survive active removal");
    assert!(Arc::ptr_eq(&canonical, &during_active_removal));

    man.reserved.push_front(Arc::clone(&moved));
    let during_reserved_insert = man
        .find_group(gid)
        .expect("lookup must survive reserved insertion");
    assert!(Arc::ptr_eq(&canonical, &during_reserved_insert));

    let moved_back = man.reserved.pop_front().expect("group must be reserved");
    let during_reserved_removal = man
        .find_group(gid)
        .expect("lookup must survive reserved removal");
    assert!(Arc::ptr_eq(&canonical, &during_reserved_removal));
    man.active.insert(gid, moved_back);
    assert_eq!(man.count(), 1);
}

#[test]
fn test_group_snapshots_preserve_active_first_and_reserved_fifo() {
    let man = RequestGroupMan::new();
    man.set_max_concurrent(1);
    let first = man
        .add_group(
            vec!["http://example.com/first.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let second = man
        .add_group(
            vec!["http://example.com/second.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let third = man
        .add_group(
            vec!["http://example.com/third.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

    assert_eq!(man.fill_from_reserver().len(), 1);
    let gids: Vec<_> = man.all_groups().into_iter().map(|(gid, _)| gid).collect();
    assert_eq!(gids, vec![first, second, third]);

    let waiting: Vec<_> = man
        .get_waiting_groups()
        .into_iter()
        .map(|group| group.recover().gid())
        .collect();
    assert_eq!(waiting, vec![second, third]);

    let active = man.active.remove(&first).expect("group must be active").1;
    let during_transfer: Vec<_> = man.all_groups().into_iter().map(|(gid, _)| gid).collect();
    assert_eq!(during_transfer, vec![second, third, first]);

    man.reserved.push_front(active);
    let after_requeue: Vec<_> = man.all_groups().into_iter().map(|(gid, _)| gid).collect();
    assert_eq!(after_requeue, vec![first, second, third]);
}

/// Test download_finished check.

#[test]
fn test_download_finished() {
    let man = RequestGroupMan::new();
    assert!(man.download_finished());
    man.add_group(
        vec!["http://example.com".to_string()],
        DownloadOptions::default(),
    )
    .unwrap();
    assert!(!man.download_finished());
}

// ── Pause → reserved → unpause → re-promotion loop ─────────────────

/// A paused active group (no more in-flight commands) must be re-queued
/// to the reserved queue — not demoted to stopped results — and must be
/// able to resume via unpause → promotion. This is the core "pause then
/// unpause then resume" closed loop.

#[test]
fn test_paused_group_requeues_to_reserved_and_can_resume() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

    // Promote to active.
    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1);
    assert_eq!(man.active.len(), 1);
    assert_eq!(man.reserved.len(), 0);

    // aria2.pause: status → Paused (num_commands is 0 because no real
    // task was spawned in this unit test).
    man.pause_group(gid).unwrap();
    let group = man.find_group(gid).unwrap();
    assert!(group.recover().status().is_paused());

    // The paused group returns to the reserved queue.
    let requeued = man.requeue_non_terminal_groups(None);
    assert_eq!(requeued, 1, "paused group should be re-queued");
    assert_eq!(man.active.len(), 0);
    assert_eq!(man.reserved.len(), 1);
    assert!(
        man.find_group(gid).is_some(),
        "paused group must still exist"
    );

    // Unpause then promote → the download restarts.
    man.unpause_group(gid).unwrap();
    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1, "unpaused group must be re-promoted");
    assert_eq!(man.active_count(), 1);
    let status = man.find_group(gid).unwrap().recover().status();
    assert_eq!(status, DownloadStatus::Active);
}

#[test]
fn paused_group_with_inflight_command_keeps_its_concurrency_slot() {
    let man = RequestGroupMan::new();
    man.set_max_concurrent(1);
    let first = man
        .add_group(
            vec!["http://example.com/first.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let second = man
        .add_group(
            vec!["http://example.com/second.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

    assert_eq!(man.fill_from_reserver().len(), 1);
    let first_group = man.find_group(first).unwrap();
    first_group.recover().inc_commands();
    man.pause_group(first).unwrap();

    assert_eq!(man.active_count(), 1);
    assert!(
        man.fill_from_reserver().is_empty(),
        "a paused command still draining must retain its active slot"
    );
    assert!(man.find_group(second).is_some());
    assert_eq!(man.reserved.len(), 1);
}

#[test]
fn test_paused_reserved_group_is_not_promoted_until_unpaused() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

    // Reproduce the race window between the pause-flag check and the
    // promotion status transition: the status is paused, but the flag
    // has already been consumed by another lifecycle operation.
    let group = man.find_group(gid).unwrap();
    {
        let mut group = group.recover_mut();
        group.pause().unwrap();
        group.control_flags.clear_pause();
    }

    let promoted = man.fill_from_reserver();
    assert!(promoted.is_empty(), "paused group must remain reserved");
    assert_eq!(man.reserved.len(), 1);
    assert_eq!(man.active.len(), 0);
    assert!(man.find_group(gid).unwrap().recover().status().is_paused());

    man.unpause_group(gid).unwrap();
    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1, "unpaused group should be promoted");
    assert_eq!(man.active_count(), 1);
}
