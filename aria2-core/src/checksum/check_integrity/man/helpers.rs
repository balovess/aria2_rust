use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::oneshot;

use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::HashType;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::RequestGroup;
use crate::util::rwlock_ext::RwLockRecover;

use super::cancelled_error;
use super::dispatch::SharedCheckIntegrityMan;
use super::queue::CheckIntegrityEntry;
use super::tasks::{
    CheckIntegrityTask, FileChecksumTask, FileChunkValidator, IntegrityOutcome,
    MultiFileChunkValidator,
};

// Entry-point helpers used by download commands
// ---------------------------------------------------------------------------

/// Queue an integrity check and return its completion receiver.
async fn enqueue_entry(
    man: &SharedCheckIntegrityMan,
    gid: u64,
    task: Box<dyn CheckIntegrityTask>,
) -> oneshot::Receiver<Result<IntegrityOutcome>> {
    let (tx, rx) = oneshot::channel();
    let entry = CheckIntegrityEntry::new(gid, task, tx);
    man.write().await.push_entry(entry);
    rx
}

/// Queue an integrity check and wait for its detailed validation outcome.
pub async fn enqueue_with_outcome(
    man: &SharedCheckIntegrityMan,
    gid: u64,
    task: Box<dyn CheckIntegrityTask>,
) -> Result<IntegrityOutcome> {
    enqueue_entry(man, gid, task)
        .await
        .await
        .map_err(|_| cancelled_error())?
}

pub async fn enqueue(
    man: &SharedCheckIntegrityMan,
    gid: u64,
    task: Box<dyn CheckIntegrityTask>,
) -> Result<bool> {
    let outcome = enqueue_entry(man, gid, task)
        .await
        .await
        .map_err(|_| cancelled_error())??;
    Ok(outcome.verified)
}

/// Build a [`FileChunkValidator`] task for a single file.
///
/// Returns `None` when there is nothing to validate (no expected digests, a
/// zero-length file, or the file does not exist yet).
pub fn multi_file_task(
    files: Vec<(PathBuf, u64)>,
    piece_length: u64,
    total_length: u64,
    expected_hex: Vec<String>,
    algo: HashType,
) -> Result<Option<Box<dyn CheckIntegrityTask>>> {
    // A missing non-empty physical file is an incomplete payload, not an
    // integrity-check I/O failure. Let the owning BT command enter its normal
    // piece-download path, matching the single-file helper's behavior.
    if expected_hex.is_empty()
        || total_length == 0
        || files.is_empty()
        || files
            .iter()
            .any(|(path, length)| *length > 0 && !path.is_file())
    {
        return Ok(None);
    }
    Ok(Some(Box::new(MultiFileChunkValidator::new(
        files,
        piece_length,
        total_length,
        expected_hex,
        algo,
    )?)))
}

/// Truncate an output file when it contains bytes beyond the declared length.
pub async fn cut_trailing_garbage(path: &Path, expected_length: u64) -> Result<()> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Aria2Error::FileIo(format!("{}: {error}", path.display()))),
    };
    if metadata.len() > expected_length {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .await
            .map_err(|error| Aria2Error::FileOpen(format!("{}: {error}", path.display())))?;
        file.set_len(expected_length)
            .await
            .map_err(|error| Aria2Error::FileIo(format!("{}: {error}", path.display())))?;
    }
    Ok(())
}

/// Truncate each physical file in a logical multi-file stream to its declared length.
pub async fn cut_multi_file_trailing_garbage(files: &[(PathBuf, u64)]) -> Result<()> {
    for (path, expected_length) in files {
        cut_trailing_garbage(path, *expected_length).await?;
    }
    Ok(())
}

pub fn file_task(
    path: &Path,
    piece_length: u64,
    total_length: u64,
    expected_hex: Vec<String>,
    algo: HashType,
) -> Result<Option<Box<dyn CheckIntegrityTask>>> {
    if expected_hex.is_empty() || total_length == 0 || !path.exists() {
        return Ok(None);
    }
    Ok(Some(Box::new(FileChunkValidator::new(
        path.to_path_buf(),
        piece_length,
        total_length,
        expected_hex,
        algo,
    )?)))
}

/// Queue a whole-file checksum validation through the shared lifecycle-aware
/// integrity dispatcher.
pub async fn enqueue_file_checksum_for_group(
    man: &SharedCheckIntegrityMan,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    path: &Path,
    total_length: u64,
    checksum: Checksum,
) -> Result<bool> {
    let outcome = enqueue_with_outcome_for_group(
        man,
        group,
        Box::new(FileChecksumTask::new(
            path.to_path_buf(),
            total_length,
            checksum,
        )),
    )
    .await?;
    Ok(outcome.verified)
}

/// Cancel all pending checks and notify their waiters (engine shutdown).
pub async fn cancel_all(man: &SharedCheckIntegrityMan) {
    man.write().await.cancel_all();
}

/// Cancel integrity validation for one RequestGroup.
pub async fn cancel_gid(man: &SharedCheckIntegrityMan, gid: u64) -> bool {
    man.write().await.cancel_gid(gid)
}

fn request_group_cancellation_error(group: &RequestGroup) -> Option<Aria2Error> {
    if group.is_removed() {
        Some(Aria2Error::DownloadFailed(
            "Download cancelled by user".to_string(),
        ))
    } else if group.is_paused_flag() {
        Some(Aria2Error::DownloadFailed("Download paused".to_string()))
    } else if group.is_force_halt_requested() || group.is_halt_requested() {
        Some(Aria2Error::DownloadFailed("Download halted".to_string()))
    } else {
        None
    }
}

/// Queue an integrity check while observing its owning RequestGroup.
///
/// The validator worker remains the owner of validation state, but lifecycle
/// control belongs to the RequestGroup. The group's lifecycle notification
/// wakes this waiter immediately when pause/remove/halt changes state.
pub async fn enqueue_with_outcome_for_group(
    man: &SharedCheckIntegrityMan,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    task: Box<dyn CheckIntegrityTask>,
) -> Result<IntegrityOutcome> {
    let gid = group.recover().gid().value();
    let lifecycle_notify = group.recover().lifecycle_notifier();
    // Queue before observing lifecycle state so cancellation can always find
    // and complete the entry, even when the group was already stopped before
    // this function was first polled.
    let receiver = enqueue_entry(man, gid, task).await;
    let mut validation = Box::pin(async move { receiver.await.map_err(|_| cancelled_error())? });

    loop {
        let lifecycle_changed = lifecycle_notify.notified();
        tokio::pin!(lifecycle_changed);
        lifecycle_changed.as_mut().enable();

        let cancellation_error = {
            let group_guard = group.recover();
            request_group_cancellation_error(&group_guard)
        };
        if let Some(error) = cancellation_error {
            cancel_gid(man, gid).await;
            let _ = validation.await;
            return Err(error);
        }

        tokio::select! {
            result = &mut validation => return result,
            _ = &mut lifecycle_changed => {}
        }
    }
}

#[cfg(test)]
mod trailing_garbage_tests {
    use super::{cut_multi_file_trailing_garbage, cut_trailing_garbage, multi_file_task};
    use crate::checksum::message_digest::HashType;

    #[tokio::test]
    async fn truncates_single_file_only_when_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.bin");
        tokio::fs::write(&path, vec![0u8; 12]).await.unwrap();
        cut_trailing_garbage(&path, 8).await.unwrap();
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().len(), 8);
        cut_trailing_garbage(&path, 8).await.unwrap();
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().len(), 8);
    }

    #[tokio::test]
    async fn truncates_each_multi_file_entry() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        tokio::fs::write(&first, vec![0u8; 13]).await.unwrap();
        tokio::fs::write(&second, vec![0u8; 7]).await.unwrap();
        cut_multi_file_trailing_garbage(&[(first.clone(), 5), (second.clone(), 3)])
            .await
            .unwrap();
        assert_eq!(tokio::fs::metadata(first).await.unwrap().len(), 5);
        assert_eq!(tokio::fs::metadata(second).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn skips_multi_file_integrity_task_when_payload_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.bin");
        let missing = dir.path().join("missing.bin");
        tokio::fs::write(&present, b"abcd").await.unwrap();

        let task = multi_file_task(
            vec![(present, 4), (missing, 4)],
            4,
            8,
            vec!["00".to_string(); 2],
            HashType::Sha1,
        )
        .unwrap();

        assert!(task.is_none());
    }
}
