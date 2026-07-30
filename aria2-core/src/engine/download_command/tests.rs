use std::sync::Arc;

use crate::engine::command::ProgressUpdate;
use crate::engine::download_command::DownloadCommand;
use crate::error::Aria2Error;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

impl DownloadCommand {
    fn has_progress_sender(&self) -> bool {
        self.progress_sender.is_some()
    }

    fn has_progress_receiver(&self) -> bool {
        self.progress_receiver.is_some()
    }

    fn has_progress_aggregator_handle(&self) -> bool {
        self.progress_aggregator_handle.is_some()
    }

    fn send_progress_update(&self, update: ProgressUpdate) {
        if let Some(ref sender) = self.progress_sender {
            let _ = sender.send(update);
        } else {
            panic!("test called send_progress_update but no sender is set");
        }
    }
}

#[test]
fn test_progress_channel_auto_created() {
    let cmd = DownloadCommand::new(
        GroupId::new(1),
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new should succeed with a valid HTTP URI");

    assert!(
        cmd.has_progress_sender(),
        "progress_sender should be Some after construction (auto-created)"
    );
    assert!(
        cmd.has_progress_receiver(),
        "progress_receiver should be Some after construction (held for lazy spawn)"
    );
    assert!(
        !cmd.has_progress_aggregator_handle(),
        "progress_aggregator_handle should be None until execute() spawns it"
    );
}

#[tokio::test]
async fn test_progress_updates_flow_through_channel() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));
    let group_clone = Arc::clone(&group);

    let mut cmd = DownloadCommand::new_with_group(
        group,
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    assert!(cmd.has_progress_sender());
    assert!(cmd.has_progress_receiver());

    cmd.spawn_progress_aggregator();
    assert!(cmd.has_progress_aggregator_handle());
    assert!(!cmd.has_progress_receiver());

    cmd.send_progress_update(ProgressUpdate {
        completed_bytes: 4096,
        download_speed: 0,
        upload_speed: 0,
    });

    cmd.drain_progress_aggregator().await;
    assert!(!cmd.has_progress_sender());
    assert!(!cmd.has_progress_aggregator_handle());

    let completed = { group_clone.recover().get_completed_length() };
    assert_eq!(
        completed, 4096,
        "aggregator should have applied the progress update to RequestGroup"
    );
}

/// Verify that check_cancelled() returns Ok(()) for a fresh group
/// (status = Waiting) and Err(DownloadFailed) after the group is
/// marked Removed.
#[tokio::test]
async fn test_check_cancelled_returns_ok_for_active_group() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(10),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));

    let cmd = DownloadCommand::new_with_group(
        group,
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    // Fresh group (Waiting status) -- not cancelled.
    assert!(
        cmd.check_cancelled().is_ok(),
        "check_cancelled() should return Ok for a fresh (non-removed) group"
    );
}

#[tokio::test]
async fn test_check_cancelled_returns_err_after_remove() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(11),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));

    let cmd = DownloadCommand::new_with_group(
        Arc::clone(&group),
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    // Simulate aria2.remove / aria2.forceRemove which calls
    // RequestGroupMan::remove_group -> group.remove().
    {
        let mut g = group.recover_mut();
        g.remove().unwrap();
    }

    let err = cmd
        .check_cancelled()
        .expect_err("check_cancelled() should return Err after the group is marked Removed");
    assert!(
        matches!(err, Aria2Error::DownloadFailed(_)),
        "expected DownloadFailed error, got {:?}",
        err
    );
}
