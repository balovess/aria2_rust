use super::*;

#[tokio::test]
async fn graceful_halt_exits_even_in_keep_alive_mode() {
    // Regression: `halt_requested` used to be write-only, so the exit
    // condition `(all_done && !keep_alive) || force_halt` could never fire
    // under `--enable-rpc` and `aria2.shutdown` hung forever.
    let (tx, rx) = mpsc::unbounded_channel();
    let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

    tx.send(EngineCommand::HaltAll {
        reason: HaltReason::ShutdownSignal,
    })
    .unwrap();

    run_until_exit(test_ctx(true), rx, sd_rx, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn force_halt_exits_in_keep_alive_mode() {
    let (tx, rx) = mpsc::unbounded_channel();
    let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

    tx.send(EngineCommand::ForceHaltAll {
        reason: HaltReason::ShutdownSignal,
    })
    .unwrap();

    run_until_exit(test_ctx(true), rx, sd_rx, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn force_halt_removes_reserved_groups_before_exit() {
    let ctx = test_ctx(true);
    let gid = ctx
        .group_man
        .add_group(
            vec!["http://example.com/queued-before-force-halt.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let group_man = Arc::clone(&ctx.group_man);

    let (tx, rx) = mpsc::unbounded_channel();
    let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();
    tx.send(EngineCommand::ForceHaltAll {
        reason: HaltReason::ShutdownSignal,
    })
    .unwrap();

    run_until_exit(ctx, rx, sd_rx, Duration::from_secs(5)).await;

    assert_eq!(group_man.count(), 0, "force halt must remove queued groups");
    assert!(group_man.find_group(gid).is_none());
    assert_eq!(group_man.stopped_results_len(), 1);
    let result = group_man
        .find_stopped_result(&gid.to_hex_string())
        .expect("queued force-halted group should have a stopped result");
    assert_eq!(result.status, DownloadStatus::Removed);
    assert_eq!(result.code, DownloadResultCode::Removed);
}

#[tokio::test]
async fn force_halt_wakes_file_allocation_waiter_before_protocol_timeout() {
    use crate::filesystem::file_allocation::AllocationStrategy;
    use crate::filesystem::file_allocation_man::enqueue_path;
    use tokio::sync::oneshot;

    let mut ctx = test_ctx(true);
    let file_alloc_man = Arc::new(tokio::sync::RwLock::new(FileAllocationMan::new()));
    ctx.file_alloc_man = Arc::clone(&file_alloc_man);
    let gid = GroupId::new(703);
    let path = std::env::temp_dir().join(format!(
        "aria2-force-halt-allocation-{}",
        std::process::id()
    ));
    let (done_tx, done_rx) = oneshot::channel();
    let allocation_task = tokio::spawn({
        let file_alloc_man = Arc::clone(&file_alloc_man);
        async move {
            let result = enqueue_path(
                &file_alloc_man,
                &path,
                4096,
                AllocationStrategy::Trunc,
                false,
                gid.value(),
            )
            .await;
            let _ = done_tx.send(result.is_err());
        }
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if file_alloc_man.read().await.is_queued_gid(gid.value()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("allocation waiter should enter the queue");

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    cmd_tx
        .send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .unwrap();
    let mut cmd_rx = EngineCommandReceiver::from_unbounded(cmd_rx);
    let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
    let mut running_downloads = vec![(
        gid,
        RunningDownload {
            _handle: allocation_task,
            shutdown: Some(CancellationToken::new()),
            generation: 1,
            last_activity: Instant::now(),
            timeout: None,
        },
    )];
    let mut halt_requested = false;
    let mut force_halt_requested = false;

    tokio::time::timeout(
        Duration::from_millis(500),
        process_engine_commands(
            &mut ctx,
            &mut cmd_rx,
            &mut running_downloads,
            &mut halt_requested,
            &mut force_halt_requested,
            &completion_tx,
        ),
    )
    .await
    .expect("force halt must wake allocation waiters before timeout");

    assert!(
        done_rx
            .await
            .expect("allocation task should report cancellation")
    );
    assert!(running_downloads.is_empty());
    assert!(!file_alloc_man.read().await.is_queued_gid(gid.value()));
}

#[tokio::test]
async fn force_halt_accounts_for_aborted_running_task() {
    let mut ctx = test_ctx(true);
    let gid = ctx
        .group_man
        .add_group(
            vec!["http://example.com/force-halt-running.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let group = ctx.group_man.fill_from_reserver().remove(0);
    group.recover().inc_commands();

    let handle = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    let mut running_downloads = vec![(
        gid,
        RunningDownload {
            _handle: handle,
            shutdown: Some(CancellationToken::new()),
            generation: 41,
            last_activity: Instant::now(),
            timeout: None,
        },
    )];
    let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    cmd_tx
        .send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .unwrap();
    let mut cmd_rx = EngineCommandReceiver::from_unbounded(cmd_rx);
    let mut halt_requested = false;
    let mut force_halt_requested = false;

    process_engine_commands(
        &mut ctx,
        &mut cmd_rx,
        &mut running_downloads,
        &mut halt_requested,
        &mut force_halt_requested,
        &completion_tx,
    )
    .await;

    let mut completed_generations = HashSet::new();
    let processed = process_task_completions(
        &ctx,
        &mut completion_rx,
        &mut running_downloads,
        &mut completed_generations,
    )
    .await;
    assert!(processed);
    assert!(running_downloads.is_empty());
    assert_eq!(completed_generations.get(&41), Some(&41));
    assert_eq!(
        ctx.group_man
            .find_group(gid)
            .unwrap()
            .recover()
            .num_commands(),
        0
    );
}

#[tokio::test]
async fn shutdown_signal_exits_in_keep_alive_mode() {
    // The Ctrl+C path sets `halt_requested` directly rather than going
    // through an EngineCommand, so it needs its own coverage.
    let (_tx, rx) = mpsc::unbounded_channel();
    let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();

    sd_tx.send(()).unwrap();

    run_until_exit(test_ctx(true), rx, sd_rx, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn shutdown_signal_preserves_active_group_for_resume() {
    let ctx = test_ctx(true);
    let gid = ctx
        .group_man
        .add_group(
            vec!["http://example.com/shutdown-resume.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    ctx.group_man.fill_from_reserver();
    let group_man = Arc::clone(&ctx.group_man);

    // Deliver the shutdown signal before the first engine wait.
    // The group is already active but has no command, so the test isolates
    // the shutdown reason from protocol-specific cancellation behavior.
    let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
    sd_tx.send(()).unwrap();
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    run_engine_loop(ctx, cmd_rx, sd_rx).await;

    let group = group_man
        .find_group(gid)
        .expect("group should remain visible");
    assert_eq!(
        group.recover().get_halt_reason(),
        HaltReason::ShutdownSignal
    );
    assert_ne!(
        group.recover().status(),
        DownloadStatus::Removed,
        "shutdown must not turn a resumable group into a user removal"
    );
    assert_eq!(
        group.recover().create_download_result().code,
        DownloadResultCode::InProgress
    );
}

#[tokio::test]
async fn keep_alive_without_halt_does_not_exit() {
    // The flip side: keep-alive must still hold the loop open when no halt
    // was requested, otherwise an idle RPC server would shut itself down.
    let (_tx, rx) = mpsc::unbounded_channel();
    let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

    let loop_fut = run_engine_loop(test_ctx(true), rx, sd_rx);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), loop_fut)
            .await
            .is_err(),
        "keep-alive loop exited without a halt request"
    );
}

#[tokio::test]
async fn idle_loop_exits_without_keep_alive() {
    let (_tx, rx) = mpsc::unbounded_channel();
    let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

    run_until_exit(test_ctx(false), rx, sd_rx, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn engine_cleanup_cancels_only_its_running_gids() {
    use crate::filesystem::file_allocation::AllocationStrategy;
    use crate::filesystem::file_allocation_man::{FileAllocationEntry, FileAllocationProtocol};
    use tokio::sync::oneshot;

    let mut ctx = test_ctx(false);
    let file_alloc_man = Arc::new(tokio::sync::RwLock::new(FileAllocationMan::new()));
    let (target_tx, target_rx) = oneshot::channel();
    let (other_tx, mut other_rx) = oneshot::channel();

    {
        let mut man = file_alloc_man.write().await;
        man.push_entry(FileAllocationEntry::single(
            701,
            std::path::PathBuf::from("/tmp/engine-cleanup-target"),
            100,
            AllocationStrategy::Trunc,
            false,
            FileAllocationProtocol::Http,
            target_tx,
        ));
        man.push_entry(FileAllocationEntry::single(
            702,
            std::path::PathBuf::from("/tmp/engine-cleanup-other"),
            100,
            AllocationStrategy::Trunc,
            false,
            FileAllocationProtocol::Http,
            other_tx,
        ));
    }
    ctx.file_alloc_man = Arc::clone(&file_alloc_man);

    let handle = tokio::spawn(async {});
    let mut running_downloads = vec![(
        GroupId::new(701),
        RunningDownload {
            _handle: handle,
            shutdown: Some(CancellationToken::new()),
            generation: 1,
            last_activity: Instant::now(),
            timeout: None,
        },
    )];

    on_end_of_run(&ctx, &mut running_downloads).await;

    assert!(target_rx.await.unwrap().is_err());
    assert!(other_rx.try_recv().is_err());
    file_alloc_man.write().await.cancel_all();
    assert!(other_rx.await.unwrap().is_err());
}
