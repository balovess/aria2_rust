use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock as TokioRwLock, oneshot};

use super::queue::{FileAllocationEntry, FileAllocationMan};
use super::{enqueue_multi, enqueue_path, shared};
use crate::filesystem::file_allocation::AllocationStrategy;

fn make_entry(gid: u64) -> FileAllocationEntry {
    let (sender, _receiver) = oneshot::channel();
    FileAllocationEntry::single(
        gid,
        PathBuf::from(format!("/tmp/test_{gid}")),
        1024 * 1024,
        AllocationStrategy::Trunc,
        false,
        sender,
    )
}

#[test]
fn queue_picks_and_drops_entries_in_order() {
    let mut manager = FileAllocationMan::new();
    assert!(!manager.has_next());
    assert!(!manager.is_picked());

    manager.push_entry(make_entry(1));
    manager.push_entry(make_entry(2));
    assert_eq!(manager.count_in_queue(), 2);

    assert_eq!(manager.take_next_owned().expect("first entry").gid, 1);
    assert!(manager.is_picked_gid(1));
    assert_eq!(manager.count_in_queue(), 1);
    assert_eq!(manager.active_count(), 1);

    manager.drop_picked();
    assert!(!manager.is_picked());
    assert_eq!(manager.take_next_owned().expect("second entry").gid, 2);
}

#[test]
fn queue_reports_gid_membership_and_total_entries() {
    let mut manager = FileAllocationMan::new();
    manager.push_entry(make_entry(1));
    manager.push_entry(make_entry(2));

    assert!(manager.is_queued_gid(1));
    assert!(!manager.is_picked_gid(1));
    assert_eq!(manager.total_entries(), 2);

    manager.take_next_owned();
    assert!(!manager.is_queued_gid(1));
    assert!(manager.is_picked_gid(1));
    assert_eq!(manager.total_entries(), 2);
}

#[test]
fn cancel_all_notifies_queued_waiters() {
    let manager = Arc::new(TokioRwLock::new(FileAllocationMan::new()));
    let (sender_a, receiver_a) = oneshot::channel();
    let (sender_b, receiver_b) = oneshot::channel();

    {
        let mut guard = manager.blocking_write();
        guard.push_entry(FileAllocationEntry::single(
            1,
            PathBuf::from("/tmp/a"),
            100,
            AllocationStrategy::Trunc,
            false,
            sender_a,
        ));
        guard.push_entry(FileAllocationEntry::single(
            2,
            PathBuf::from("/tmp/b"),
            100,
            AllocationStrategy::Trunc,
            false,
            sender_b,
        ));
    }

    manager.blocking_write().cancel_all();
    assert_eq!(manager.blocking_read().count_in_queue(), 0);
    assert!(receiver_a.blocking_recv().unwrap().is_err());
    assert!(receiver_b.blocking_recv().unwrap().is_err());
}

#[test]
fn cancel_gid_only_notifies_matching_waiters() {
    let manager = Arc::new(TokioRwLock::new(FileAllocationMan::new()));
    let (target_sender, target_receiver) = oneshot::channel();
    let (other_sender, mut other_receiver) = oneshot::channel();
    let (target_again_sender, target_again_receiver) = oneshot::channel();

    {
        let mut guard = manager.blocking_write();
        guard.push_entry(FileAllocationEntry::single(
            11,
            PathBuf::from("/tmp/target-a"),
            100,
            AllocationStrategy::Trunc,
            false,
            target_sender,
        ));
        guard.push_entry(FileAllocationEntry::single(
            12,
            PathBuf::from("/tmp/other"),
            100,
            AllocationStrategy::Trunc,
            false,
            other_sender,
        ));
        guard.push_entry(FileAllocationEntry::single(
            11,
            PathBuf::from("/tmp/target-b"),
            100,
            AllocationStrategy::Trunc,
            false,
            target_again_sender,
        ));
    }

    assert_eq!(manager.blocking_write().cancel_gid(11), 2);
    assert_eq!(manager.blocking_read().count_in_queue(), 1);
    assert!(target_receiver.blocking_recv().unwrap().is_err());
    assert!(target_again_receiver.blocking_recv().unwrap().is_err());
    assert!(other_receiver.try_recv().is_err());

    manager.blocking_write().cancel_all();
    assert!(other_receiver.blocking_recv().unwrap().is_err());
}

#[test]
fn default_manager_starts_empty() {
    let manager = FileAllocationMan::default();
    assert!(!manager.has_next());
    assert!(!manager.is_picked());
    assert_eq!(manager.total_entries(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enqueue_path_allocates_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allocated.bin");
    let manager = super::dispatch::test_manager();

    enqueue_path(&manager, &path, 4096, AllocationStrategy::Trunc, false, 7)
        .await
        .unwrap();
    assert_eq!(tokio::fs::metadata(path).await.unwrap().len(), 4096);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enqueue_multi_allocates_nested_files() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![
        (dir.path().join("a.bin"), 8192u64),
        (dir.path().join("sub/b.bin"), 4096u64),
        (dir.path().join("empty.bin"), 0u64),
    ];
    let manager = super::dispatch::test_manager();

    enqueue_multi(&manager, files.clone(), AllocationStrategy::Trunc, false, 9)
        .await
        .unwrap();

    assert_eq!(tokio::fs::metadata(&files[0].0).await.unwrap().len(), 8192);
    assert_eq!(tokio::fs::metadata(&files[1].0).await.unwrap().len(), 4096);
    assert!(!files[2].0.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preallocation_preserves_existing_prefix_and_skips_complete_file() {
    let dir = tempfile::tempdir().unwrap();
    let partial = dir.path().join("partial.bin");
    let complete = dir.path().join("complete.bin");
    let prefix = vec![0xA5u8; 4096];
    std::fs::write(&partial, &prefix).unwrap();
    std::fs::write(&complete, vec![0xABu8; 16_384]).unwrap();
    let manager = super::dispatch::test_manager();

    enqueue_multi(
        &manager,
        vec![(partial.clone(), 8192), (complete.clone(), 8192)],
        AllocationStrategy::Prealloc,
        false,
        10,
    )
    .await
    .unwrap();

    assert_eq!(tokio::fs::metadata(&partial).await.unwrap().len(), 8192);
    assert_eq!(tokio::fs::metadata(&complete).await.unwrap().len(), 16_384);
    assert_eq!(
        &tokio::fs::read(partial).await.unwrap()[..prefix.len()],
        prefix
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_keeps_fifo_order() {
    let dir = tempfile::tempdir().unwrap();
    let manager = super::dispatch::test_manager();

    let (sender_a, receiver_a) = oneshot::channel();
    manager
        .write()
        .await
        .push_entry(FileAllocationEntry::single(
            1,
            dir.path().join("big.bin"),
            2 * 1024 * 1024,
            AllocationStrategy::Prealloc,
            false,
            sender_a,
        ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (sender_b, receiver_b) = oneshot::channel();
    manager
        .write()
        .await
        .push_entry(FileAllocationEntry::single(
            2,
            dir.path().join("small.bin"),
            4096,
            AllocationStrategy::Trunc,
            false,
            sender_b,
        ));

    tokio::time::timeout(Duration::from_secs(15), receiver_a)
        .await
        .expect("first allocation must finish")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), receiver_b)
        .await
        .expect("second allocation must finish")
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::fs::metadata(dir.path().join("small.bin"))
            .await
            .unwrap()
            .len(),
        4096
    );
}

#[test]
fn shared_manager_is_process_wide() {
    let first = shared();
    let second = shared();
    assert!(Arc::ptr_eq(&first, &second));
}
