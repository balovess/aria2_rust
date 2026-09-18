use super::*;

#[test]
fn test_concurrent_add_groups() {
    let man = Arc::new(RequestGroupMan::new());
    let num_tasks = 100;
    let mut handles = vec![];

    for i in 0..num_tasks {
        let man_clone = man.clone();
        let handle = thread::spawn(move || {
            let uri = format!("http://example.com/file{}.bin", i);
            let options = DownloadOptions::default();
            man_clone.add_group(vec![uri], options)
        });
        handles.push(handle);
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join()).collect();

    for result in results {
        assert!(result.is_ok());
        let gid = result.unwrap().unwrap();
        assert!(gid.value() > 0);
    }

    assert_eq!(man.count(), num_tasks);
}

#[test]
fn stale_terminal_add_command_does_not_reinsert_group() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let group = man.find_group(gid).unwrap();

    // A queued AddDownload can outlive a concurrent remove. Replaying it
    // must not put a terminal group back into the canonical index.
    man.remove_group(gid).unwrap();
    assert!(man.find_group(gid).is_none());

    man.add_group_arc(group);

    assert!(man.find_group(gid).is_none());
    assert_eq!(man.reserved.len(), 0);
    assert_eq!(man.stopped_count(), 1);
}

#[test]
fn add_group_arc_marks_memory_download_from_options() {
    let man = RequestGroupMan::new();
    let options = DownloadOptions {
        follow_metalink: Some(crate::request::request_group::FollowMode::Memory),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(73),
        vec!["http://example.com/index.meta4".to_string()],
        options,
    )));

    assert!(!group.recover().is_in_memory_download());
    man.add_group_arc(Arc::clone(&group));

    assert!(
        group.recover().is_in_memory_download(),
        "pre-constructed groups must honor memory-backed metadata options"
    );
}

#[test]
fn newly_queued_groups_honor_initial_pause_option() {
    let man = RequestGroupMan::new();
    let paused_options = DownloadOptions {
        pause: true,
        ..DownloadOptions::default()
    };

    let generated_gid = man
        .add_group(
            vec!["http://example.com/generated.bin".to_string()],
            paused_options.clone(),
        )
        .unwrap();
    assert!(
        man.find_group(generated_gid)
            .unwrap()
            .recover()
            .status()
            .is_paused()
    );

    let explicit_gid = GroupId::new(74);
    man.add_group_with_gid(
        explicit_gid,
        vec!["http://example.com/explicit.bin".to_string()],
        paused_options,
    )
    .unwrap();
    assert!(
        man.find_group(explicit_gid)
            .unwrap()
            .recover()
            .status()
            .is_paused()
    );
}

#[test]
fn control_file_save_requests_skip_terminal_groups() {
    let man = RequestGroupMan::new();
    let gids: Vec<_> = (0..6)
        .map(|index| {
            man.add_group(
                vec![format!("http://example.com/file{index}.bin")],
                DownloadOptions::default(),
            )
            .unwrap()
        })
        .collect();

    man.fill_from_reserver();
    man.find_group(gids[1])
        .unwrap()
        .recover_mut()
        .pause()
        .unwrap();
    man.find_group(gids[2]).unwrap().recover().mark_complete();
    man.find_group(gids[3])
        .unwrap()
        .recover()
        .mark_error("failed".to_string());
    man.find_group(gids[4]).unwrap().recover().mark_removed();

    man.request_control_file_saves();

    assert!(
        man.find_group(gids[0])
            .unwrap()
            .recover()
            .is_save_control_file_requested()
    );
    assert!(
        man.find_group(gids[1])
            .unwrap()
            .recover()
            .is_save_control_file_requested()
    );
    assert!(
        man.find_group(gids[5])
            .unwrap()
            .recover()
            .is_save_control_file_requested()
    );
    for gid in &gids[2..5] {
        assert!(
            !man.find_group(*gid)
                .unwrap()
                .recover()
                .is_save_control_file_requested()
        );
    }
}

#[test]
fn registered_group_receives_session_transfer_counters() {
    let man = RequestGroupMan::new();
    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let group = man.find_group(gid).expect("registered group");
    let stats = group
        .recover()
        .global_net_stat()
        .expect("manager counters must be injected");

    stats.update_download(7);

    assert_eq!(stats.session_download_length_for_test(), 7);
}

#[tokio::test]
async fn activity_signal_wakes_for_registration_and_progress_changes() {
    let man = RequestGroupMan::new();
    let activity = man.activity_signal();
    let mut observed = activity.generation();

    let gid = man
        .add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        activity.wait_for_change(&mut observed),
    )
    .await
    .expect("group registration must wake activity observers");
    assert!(man.find_group(gid).is_some());

    let group = man.find_group(gid).expect("registered group");
    let previous_generation = observed;
    group.recover().update_progress(1);

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        activity.wait_for_change(&mut observed),
    )
    .await
    .expect("progress changes must wake activity observers");
    assert!(observed > previous_generation);
    assert_eq!(group.recover().get_completed_length(), 1);
}
