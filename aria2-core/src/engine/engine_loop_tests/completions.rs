use super::super::completions::map_error_code;
use super::*;

#[tokio::test]
async fn paused_task_failure_keeps_group_paused() {
    let ctx = test_ctx(false);

    let gid = {
        let man = &ctx.group_man;
        man.add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap()
    };

    // Promote to active (fill_from_reserver calls start() → Active).
    {
        let man = &ctx.group_man;
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        assert!(man.find_group(gid).is_some());
    }

    // aria2.pause marks the group Paused.
    {
        let man = &ctx.group_man;
        man.pause_group(gid).unwrap();
        let g = man.find_group(gid).unwrap();
        assert!(g.recover().status().is_paused());
        // Simulate a spawned task that has not yet reported completion.
        g.recover().inc_commands();
    }

    // The download command terminates because it observed the pause.
    let (completion_tx, mut completion_rx) =
        mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
    completion_tx
        .send((
            gid,
            1,
            TaskResult::Failed(Aria2Error::DownloadFailed("Download paused".into())),
        ))
        .unwrap();

    let mut running_downloads = Vec::new();
    let mut completed_generations = HashSet::new();
    process_task_completions(
        &ctx,
        &mut completion_rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    let status = {
        let man = &ctx.group_man;
        man.find_group(gid).unwrap().recover().status()
    };
    assert_eq!(
        status,
        DownloadStatus::Paused,
        "a pause-induced task failure must keep the group Paused (resumable), not Error"
    );
}

#[tokio::test]
async fn paused_task_success_keeps_group_paused() {
    let ctx = test_ctx(false);
    let gid = {
        let man = &ctx.group_man;
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();
        man.pause_group(gid).unwrap();
        man.find_group(gid).unwrap().recover().inc_commands();
        gid
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
    tx.send((gid, 1, TaskResult::Success)).unwrap();

    let mut running_downloads = Vec::new();
    let mut completed_generations = HashSet::new();
    process_task_completions(
        &ctx,
        &mut rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    let status = ctx.group_man.find_group(gid).unwrap().recover().status();
    assert_eq!(
        status,
        DownloadStatus::Paused,
        "a clean command completion must not make a paused group terminal"
    );
}

#[tokio::test]
async fn failed_network_task_marks_only_the_observed_dns_peer_bad() {
    let ctx = test_ctx(false);
    let gid = ctx
        .group_man
        .add_group(
            vec!["http://localhost/dns-peer.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let group = ctx.group_man.find_group(gid).unwrap();
    group.recover().inc_commands();

    let cached = ctx
        .dns_cache
        .lock()
        .await
        .resolve("localhost", 80)
        .await
        .expect("localhost should resolve for the DNS cache fixture");
    let observed = cached[0];
    group
        .recover()
        .set_connection_context(ConnectionContext::new("localhost", 80, observed));

    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send((
        gid,
        1,
        TaskResult::FailedWithContext {
            error: Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "connection reset".into(),
            }),
            connection_context: ConnectionContext::new("localhost", 80, observed),
        },
    ))
    .unwrap();

    process_task_completions(&ctx, &mut rx, &mut Vec::new(), &mut HashSet::new()).await;

    let remaining = ctx
        .dns_cache
        .lock()
        .await
        .resolve_no_network("localhost", 80);
    if let Ok(addresses) = remaining {
        assert!(
            !addresses.contains(&observed),
            "the peer that actually failed must not remain a good candidate"
        );
    }
}

#[tokio::test]
async fn async_dns_disabled_does_not_modify_shared_dns_cache_on_failure() {
    let ctx = test_ctx(false);
    let gid = ctx
        .group_man
        .add_group(
            vec!["http://localhost/dns-peer-disabled.bin".to_string()],
            DownloadOptions {
                async_dns: false,
                ..DownloadOptions::default()
            },
        )
        .unwrap();
    let group = ctx.group_man.find_group(gid).unwrap();
    group.recover().inc_commands();

    let cached = ctx
        .dns_cache
        .lock()
        .await
        .resolve("localhost", 80)
        .await
        .expect("localhost should resolve for the DNS cache fixture");
    let observed = cached[0];
    group
        .recover()
        .set_connection_context(ConnectionContext::new("localhost", 80, observed));

    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send((
        gid,
        1,
        TaskResult::FailedWithContext {
            error: Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "connection reset".into(),
            }),
            connection_context: ConnectionContext::new("localhost", 80, observed),
        },
    ))
    .unwrap();

    process_task_completions(&ctx, &mut rx, &mut Vec::new(), &mut HashSet::new()).await;

    let remaining = ctx
        .dns_cache
        .lock()
        .await
        .resolve_no_network("localhost", 80)
        .expect("async-dns=false must leave the cached address available");
    assert!(
        remaining.contains(&observed),
        "async-dns=false must not mark a failed connection bad in the shared cache"
    );
}

#[tokio::test]
async fn user_removal_wins_over_paused_status_on_cancelled_task() {
    let ctx = test_ctx(false);
    let gid = {
        let man = &ctx.group_man;
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();
        let group = man.find_group(gid).unwrap();
        group.recover_mut().pause().unwrap();
        group.recover().inc_commands();
        // Force removal can arrive after a pause command has already
        // published the Paused status. The user halt reason is terminal.
        group.recover().request_force_halt(HaltReason::UserRequest);
        gid
    };

    let (completion_tx, mut completion_rx) =
        mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
    completion_tx.send((gid, 1, TaskResult::Cancelled)).unwrap();

    let mut running_downloads = Vec::new();
    let mut completed_generations = HashSet::new();
    process_task_completions(
        &ctx,
        &mut completion_rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    let status = ctx.group_man.find_group(gid).unwrap().recover().status();
    assert_eq!(status, DownloadStatus::Removed);
}

#[tokio::test]
async fn duplicate_completion_decrements_command_once() {
    let ctx = test_ctx(false);
    let gid = {
        let man = &ctx.group_man;
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();
        man.find_group(gid).unwrap().recover().inc_commands();
        gid
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
    tx.send((
        gid,
        1,
        TaskResult::Failed(Aria2Error::Network("failed".into())),
    ))
    .unwrap();
    tx.send((
        gid,
        1,
        TaskResult::Failed(Aria2Error::Network("duplicate".into())),
    ))
    .unwrap();

    let mut running_downloads: Vec<(GroupId, RunningDownload)> = Vec::new();
    let mut completed_generations: HashSet<CommandGeneration> = HashSet::new();
    process_task_completions(
        &ctx,
        &mut rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    let man = &ctx.group_man;
    let group = man.find_group(gid).unwrap();
    assert_eq!(group.recover().num_commands(), 0);
    assert_eq!(completed_generations.len(), 1);
}

#[tokio::test]
async fn same_gid_commands_have_independent_completion_generations() {
    let ctx = test_ctx(false);
    let gid = {
        let man = &ctx.group_man;
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();
        let group = man.find_group(gid).unwrap();
        group.recover().inc_commands();
        group.recover().inc_commands();
        gid
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
    tx.send((
        gid,
        1,
        TaskResult::Failed(Aria2Error::Network("first command".into())),
    ))
    .unwrap();
    tx.send((
        gid,
        2,
        TaskResult::Failed(Aria2Error::Network("second command".into())),
    ))
    .unwrap();

    let mut running_downloads = Vec::new();
    let mut completed_generations = HashSet::new();
    process_task_completions(
        &ctx,
        &mut rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    let man = &ctx.group_man;
    assert_eq!(man.find_group(gid).unwrap().recover().num_commands(), 0);
    assert_eq!(completed_generations.len(), 2);
}

#[tokio::test]
async fn non_final_command_failure_waits_for_final_completion() {
    let ctx = test_ctx(false);
    let gid = {
        let man = &ctx.group_man;
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();
        let group = man.find_group(gid).unwrap();
        group.recover().inc_commands();
        group.recover().inc_commands();
        gid
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
    tx.send((
        gid,
        1,
        TaskResult::Failed(Aria2Error::Network("first command failed".into())),
    ))
    .unwrap();

    let mut running_downloads = Vec::new();
    let mut completed_generations = HashSet::new();
    process_task_completions(
        &ctx,
        &mut rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    {
        let man = &ctx.group_man;
        let group = man.find_group(gid).unwrap();
        assert_eq!(group.recover().num_commands(), 1);
        assert!(matches!(group.recover().status(), DownloadStatus::Active));
    }

    tx.send((gid, 2, TaskResult::Success)).unwrap();
    process_task_completions(
        &ctx,
        &mut rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    let man = &ctx.group_man;
    let group = man.find_group(gid).unwrap();
    assert_eq!(group.recover().num_commands(), 0);
    assert!(matches!(group.recover().status(), DownloadStatus::Error(_)));
}

#[test]
fn error_code_mapping_preserves_aria2_semantics() {
    assert_eq!(
        map_error_code(&Aria2Error::Recoverable(RecoverableError::Timeout)),
        DownloadResultCode::TimeOut
    );
    assert_eq!(
        map_error_code(&Aria2Error::Recoverable(RecoverableError::CannotResume)),
        DownloadResultCode::CannotResume
    );
    assert_eq!(
        map_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 404
        })),
        DownloadResultCode::ResourceNotFound
    );
    assert_eq!(
        map_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 500
        })),
        DownloadResultCode::HttpProtocolError
    );
    for code in [502, 503, 504] {
        assert_eq!(
            map_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
                code
            })),
            DownloadResultCode::HttpServiceUnavailable
        );
    }
    for code in [401, 407] {
        assert_eq!(
            map_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
                code
            })),
            DownloadResultCode::HttpAuthFailed
        );
    }
    assert_eq!(
        map_error_code(&Aria2Error::Recoverable(
            RecoverableError::HttpTooManyRedirects { count: 20 }
        )),
        DownloadResultCode::HttpTooManyRedirects
    );
    assert_eq!(
        map_error_code(&Aria2Error::Checksum("bad digest".into())),
        DownloadResultCode::ChecksumError
    );
    assert_eq!(
        map_error_code(&Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: "550".into(),
            }
        )),
        DownloadResultCode::FtpProtocolError
    );
    assert_eq!(
        map_error_code(&Aria2Error::Io("disk write".into())),
        DownloadResultCode::FileIoError
    );
    assert_eq!(
        map_error_code(&Aria2Error::Fatal(
            crate::error::FatalError::DiskSpaceExhausted
        )),
        DownloadResultCode::NotEnoughDiskSpace
    );
    assert_eq!(
        map_error_code(&Aria2Error::HttpProtocol("bad status".into())),
        DownloadResultCode::HttpProtocolError
    );
    assert_eq!(
        map_error_code(&Aria2Error::FtpProtocol("bad PASV".into())),
        DownloadResultCode::FtpProtocolError
    );
    assert_eq!(
        map_error_code(&Aria2Error::DirCreate("permission denied".into())),
        DownloadResultCode::DirCreateError
    );
    assert_eq!(
        map_error_code(&Aria2Error::FileOpen("cannot open".into())),
        DownloadResultCode::FileOpenError
    );
}

#[tokio::test]
async fn genuine_task_failure_still_marks_error() {
    let ctx = test_ctx(false);

    let gid = {
        let man = &ctx.group_man;
        man.add_group(
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap()
    };
    {
        let man = &ctx.group_man;
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        let g = man.find_group(gid).unwrap();
        g.recover().inc_commands();
    }

    let (completion_tx, mut completion_rx) =
        mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
    completion_tx
        .send((
            gid,
            1,
            TaskResult::Failed(Aria2Error::Network("connection refused".into())),
        ))
        .unwrap();

    let mut running_downloads = Vec::new();
    let mut completed_generations = HashSet::new();
    process_task_completions(
        &ctx,
        &mut completion_rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;

    let status = {
        let man = &ctx.group_man;
        man.find_group(gid).unwrap().recover().status()
    };
    assert!(
        matches!(status, DownloadStatus::Error(_)),
        "a genuine network failure must still record an Error, got {:?}",
        status
    );
}
