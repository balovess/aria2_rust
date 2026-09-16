use super::{
    CheckIntegrityEntry, CheckIntegrityMan, CheckIntegrityTask, FileChunkValidator,
    IntegrityOutcome, MultiFileChunkValidator, enqueue, enqueue_file_checksum_for_group,
    enqueue_with_outcome, enqueue_with_outcome_for_group, file_task, multi_file_task, shared,
    shared_with_concurrency,
};
use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::{HashType, MessageDigest};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{GroupId, RequestGroup};
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, oneshot};

struct SlowIntegrityTask {
    remaining_chunks: usize,
    current_length: u64,
}

#[async_trait]
impl CheckIntegrityTask for SlowIntegrityTask {
    fn total_length(&self) -> u64 {
        (self.remaining_chunks as u64 + 1) * 1024
    }

    fn current_length(&self) -> u64 {
        self.current_length
    }

    fn is_finished(&self) -> bool {
        self.remaining_chunks == 0
    }

    async fn validate_chunk(&mut self) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.remaining_chunks -= 1;
        self.current_length += 1024;
        Ok(())
    }

    fn passed(&self) -> bool {
        self.is_finished()
    }
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aria2_ci_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run_active_cancellation(
    trigger: impl FnOnce(&Arc<std::sync::RwLock<RequestGroup>>),
) -> Result<IntegrityOutcome> {
    let man = shared_with_concurrency(1);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(5),
        vec!["http://example.test/payload".to_string()],
        crate::request::request_group::DownloadOptions::default(),
    )));
    let man_for_validation = Arc::clone(&man);
    let group_for_validation = Arc::clone(&group);
    let validation = tokio::spawn(async move {
        enqueue_with_outcome_for_group(
            &man_for_validation,
            group_for_validation,
            Box::new(SlowIntegrityTask {
                remaining_chunks: 100,
                current_length: 0,
            }),
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if man.read().await.is_picked() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("integrity validation should become active");

    trigger(&group);
    let result = tokio::time::timeout(Duration::from_secs(1), validation)
        .await
        .expect("lifecycle cancellation should be prompt")
        .expect("validation task should not panic");
    assert_eq!(man.read().await.active_count(), 0);
    result
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

#[tokio::test]
async fn test_multi_file_task_piece_crosses_file_boundary() {
    let dir = test_dir("multi_cross");
    let first = dir.join("first");
    let second = dir.join("second");
    tokio::fs::write(&first, b"abcd").await.unwrap();
    tokio::fs::write(&second, b"efghij").await.unwrap();
    let expected = vec![sha1_hex(b"abcdef"), sha1_hex(b"ghij")];
    let task = multi_file_task(
        vec![(first, 4), (second, 6)],
        6,
        10,
        expected,
        HashType::Sha1,
    )
    .unwrap()
    .unwrap();
    assert!(
        enqueue(&shared_with_concurrency(1), 90, task)
            .await
            .unwrap()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_multi_file_task_truncated_existing_file_is_mismatch() {
    let dir = test_dir("multi_truncated");
    let first = dir.join("first");
    let second = dir.join("second");
    tokio::fs::write(&first, b"abcd").await.unwrap();
    // The metadata declares six bytes, but the existing file contains only
    // the first two. The missing bytes must be treated as a bad piece so
    // the normal re-download path can repair the payload.
    tokio::fs::write(&second, b"ef").await.unwrap();
    let expected = vec![sha1_hex(b"abcdef"), sha1_hex(b"ghij")];
    let task = multi_file_task(
        vec![(first, 4), (second, 6)],
        6,
        10,
        expected,
        HashType::Sha1,
    )
    .unwrap()
    .unwrap();
    let outcome = enqueue_with_outcome(&shared_with_concurrency(1), 92, task)
        .await
        .expect("a truncated existing file is an integrity mismatch");

    assert!(!outcome.verified);
    assert_eq!(outcome.verified_piece_indices, vec![0]);
    assert_eq!(outcome.failed_piece_indices, vec![1]);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_multi_file_task_detects_late_file_corruption() {
    let dir = test_dir("multi_bad");
    let first = dir.join("first");
    let second = dir.join("second");
    tokio::fs::write(&first, b"abcd").await.unwrap();
    tokio::fs::write(&second, b"efghij").await.unwrap();
    let expected = vec![sha1_hex(b"abcdef"), sha1_hex(b"ghij")];
    tokio::fs::write(&second, b"efgXij").await.unwrap();
    let task = multi_file_task(
        vec![(first, 4), (second, 6)],
        6,
        10,
        expected,
        HashType::Sha1,
    )
    .unwrap()
    .unwrap();
    assert!(
        !enqueue(&shared_with_concurrency(1), 91, task)
            .await
            .unwrap()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_file_validator_passes_and_fails() {
    let dir = test_dir("valid");
    let path = dir.join("f.bin");
    // 8 bytes, 2 pieces of 4 bytes.
    let data = b"aaaabbbb".to_vec();
    std::fs::write(&path, &data).unwrap();

    let expected = vec![sha1_hex(&data[0..4]), sha1_hex(&data[4..8])];
    let man = shared_with_concurrency(1);

    // Correct digests → Ok(true).
    let task = file_task(&path, 4, 8, expected.clone(), HashType::Sha1)
        .unwrap()
        .expect("task created");
    assert!(enqueue(&man, 1, task).await.unwrap());

    // Tampered first piece → Ok(false).
    let mut bad = data.clone();
    bad[0] ^= 0xFF;
    std::fs::write(&path, &bad).unwrap();
    let task = file_task(&path, 4, 8, expected.clone(), HashType::Sha1)
        .unwrap()
        .expect("task created");
    assert!(!enqueue(&man, 2, task).await.unwrap());

    // Tampered last piece → Ok(false).
    let mut bad_last = data.clone();
    bad_last[7] ^= 0x01;
    std::fs::write(&path, &bad_last).unwrap();
    let task = file_task(&path, 4, 8, expected, HashType::Sha1)
        .unwrap()
        .expect("task created");
    assert!(!enqueue(&man, 3, task).await.unwrap());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_enqueue_with_outcome_preserves_piece_indices() {
    let dir = test_dir("outcome");
    let path = dir.join("f.bin");
    let data = b"aaaabbbb".to_vec();
    std::fs::write(&path, &data).unwrap();

    let expected = vec![sha1_hex(&data[0..4]), sha1_hex(b"xxxx")];
    let task = file_task(&path, 4, 8, expected, HashType::Sha1)
        .unwrap()
        .expect("task created");
    let outcome = enqueue_with_outcome(&shared_with_concurrency(1), 4, task)
        .await
        .unwrap();

    assert!(!outcome.verified);
    assert_eq!(outcome.verified_piece_indices, vec![0]);
    assert_eq!(outcome.failed_piece_indices, vec![1]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_file_checksum_dispatcher_streams_and_reports_mismatch() {
    let dir = test_dir("whole_file_checksum");
    let path = dir.join("payload.bin");
    let data: Vec<u8> = (0..131_072).map(|index| (index % 251) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    let expected = MessageDigest::hash_hex(HashType::Sha256, &data);
    let man = shared_with_concurrency(1);

    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(93),
        vec!["http://example.test/payload".to_string()],
        crate::request::request_group::DownloadOptions::default(),
    )));
    assert!(
        enqueue_file_checksum_for_group(
            &man,
            group,
            &path,
            data.len() as u64,
            Checksum::new(HashType::Sha256, &expected).unwrap(),
        )
        .await
        .unwrap()
    );

    let mut corrupted = data;
    corrupted[70_000] ^= 0x01;
    std::fs::write(&path, corrupted).unwrap();
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(94),
        vec!["http://example.test/payload".to_string()],
        crate::request::request_group::DownloadOptions::default(),
    )));
    assert!(
        !enqueue_file_checksum_for_group(
            &man,
            group,
            &path,
            131_072,
            Checksum::new(HashType::Sha256, &expected).unwrap(),
        )
        .await
        .unwrap()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_group_pause_cancels_active_integrity_validation() {
    let result = run_active_cancellation(|group| {
        group.write().unwrap().pause().unwrap();
    })
    .await;
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_group_remove_cancels_active_integrity_validation() {
    let result = run_active_cancellation(|group| {
        group.write().unwrap().remove().unwrap();
    })
    .await;
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download cancelled by user"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_group_halt_cancels_active_integrity_validation() {
    let result = run_active_cancellation(|group| {
        group
            .read()
            .unwrap()
            .request_halt(crate::request::request_group::HaltReason::UserRequest);
    })
    .await;
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download halted"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_group_already_paused_cancels_queued_integrity_validation() {
    let man = shared_with_concurrency(1);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(6),
        vec!["http://example.test/payload".to_string()],
        crate::request::request_group::DownloadOptions::default(),
    )));
    group.write().unwrap().pause().unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        enqueue_with_outcome_for_group(
            &man,
            group,
            Box::new(SlowIntegrityTask {
                remaining_chunks: 100,
                current_length: 0,
            }),
        ),
    )
    .await
    .expect("an already paused group must cancel queued validation promptly");

    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
    assert_eq!(man.read().await.count_in_queue(), 0);
    assert_eq!(man.read().await.active_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_file_task_none_when_no_digests_or_missing() {
    let dir = test_dir("none");
    let path = dir.join("missing.bin");

    // Missing file → None.
    assert!(
        file_task(&path, 4, 8, vec!["aa".to_string()], HashType::Sha1)
            .unwrap()
            .is_none()
    );
    // Empty digest list → None.
    let path2 = dir.join("exists.bin");
    std::fs::write(&path2, b"hello").unwrap();
    assert!(
        file_task(&path2, 4, 5, Vec::new(), HashType::Sha1)
            .unwrap()
            .is_none()
    );
    // Zero length → None.
    assert!(
        file_task(&path2, 4, 0, vec!["aa".to_string()], HashType::Sha1)
            .unwrap()
            .is_none()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_file_validator_rejects_mismatched_piece_count() {
    let result = FileChunkValidator::new(
        PathBuf::from("/tmp/payload.bin"),
        4,
        8,
        vec![sha1_hex(b"aaaa")],
        HashType::Sha1,
    );

    assert!(matches!(
        result,
        Err(Aria2Error::Parse(message)) if message.contains("digest count mismatch")
    ));
}

#[test]
fn test_multi_file_validator_rejects_mismatched_piece_count() {
    let result = MultiFileChunkValidator::new(
        vec![(PathBuf::from("/tmp/first.bin"), 8)],
        4,
        8,
        vec![sha1_hex(b"aaaa")],
        HashType::Sha1,
    );

    assert!(matches!(
        result,
        Err(Aria2Error::Parse(message)) if message.contains("digest count mismatch")
    ));
}

#[test]
fn test_queue_semantics() {
    let mut man = CheckIntegrityMan::new();
    assert!(!man.is_picked());
    assert!(!man.has_next());

    let (tx, _rx) = oneshot::channel();
    man.push_entry(CheckIntegrityEntry::new(
        1,
        Box::new(
            FileChunkValidator::new(
                PathBuf::from("/tmp/x"),
                4,
                8,
                vec![sha1_hex(b"aaaa"), sha1_hex(b"bbbb")],
                HashType::Sha1,
            )
            .unwrap(),
        ),
        tx,
    ));
    assert!(man.has_next());
    assert_eq!(man.count_in_queue(), 1);

    let entry = man.take_next_owned().expect("picked");
    assert_eq!(entry.gid, 1);
    assert!(man.is_picked());
    assert_eq!(man.active_count(), 1);

    man.drop_picked();
    assert!(!man.is_picked());
    assert_eq!(man.active_count(), 0);
}

#[test]
fn test_cancel_all_notifies_queued() {
    let man = Arc::new(RwLock::new(CheckIntegrityMan::new()));
    let (tx1, rx1) = oneshot::channel();
    {
        let mut guard = man.blocking_write();
        guard.push_entry(CheckIntegrityEntry::new(
            1,
            Box::new(
                FileChunkValidator::new(
                    PathBuf::from("/tmp/x"),
                    4,
                    8,
                    vec!["aa".to_string(), "bb".to_string()],
                    HashType::Sha1,
                )
                .unwrap(),
            ),
            tx1,
        ));
    }
    man.blocking_write().cancel_all();
    assert_eq!(man.blocking_read().count_in_queue(), 0);
    assert!(rx1.blocking_recv().unwrap().is_err());
}

#[test]
fn test_cancel_gid_notifies_only_matching_queued_entry() {
    let mut man = CheckIntegrityMan::new();
    let (target_tx, target_rx) = oneshot::channel();
    let (other_tx, mut other_rx) = oneshot::channel();

    for (gid, done_tx) in [(7, target_tx), (8, other_tx)] {
        man.push_entry(CheckIntegrityEntry::new(
            gid,
            Box::new(SlowIntegrityTask {
                remaining_chunks: 1,
                current_length: 0,
            }),
            done_tx,
        ));
    }

    assert!(man.cancel_gid(7));
    assert_eq!(man.count_in_queue(), 1);
    assert!(target_rx.blocking_recv().unwrap().is_err());
    assert!(other_rx.try_recv().is_err());
    assert_eq!(man.take_next_owned().unwrap().gid, 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_shared_instance_is_process_wide() {
    let a = shared();
    let b = shared();
    assert!(Arc::ptr_eq(&a, &b));
}
