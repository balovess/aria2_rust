//! Durable progress for sequential protocol commands.
//!
//! This is an internal Rust lifecycle seam. Protocol adapters provide the
//! output path, known total length, and completed byte count; this module
//! owns sidecar validation, checkpoint saves, cancellation flushes, and
//! successful cleanup.

use std::path::{Path, PathBuf};

use crate::filesystem::control_file::ControlFile;

const CHECKPOINT_PIECES: usize = 1;
const CHECKPOINT_SAVE_INTERVAL: u64 = 1024 * 1024;

pub(crate) struct ProgressCheckpoint {
    control_file: Option<ControlFile>,
    last_saved_length: u64,
    path: PathBuf,
}

impl ProgressCheckpoint {
    /// Return the amount of an existing output that may seed a new attempt.
    ///
    /// An explicit `continue` enables prefix reuse. A checkpoint also enables
    /// reuse because it records an interrupted Rust download lifecycle; this
    /// is what lets an unpause or a restart recover without requiring the
    /// option to be repeated. With neither signal, an existing output is a
    /// fresh-download collision and must not become an implicit resume.
    pub(crate) async fn resume_input_length(
        output_path: &Path,
        existing_length: u64,
        continue_download: bool,
        total_length: u64,
    ) -> u64 {
        let has_compatible_checkpoint = matches!(
            ControlFile::load(&ControlFile::control_path_for(output_path)).await,
            Ok(Some(control_file))
                if control_file.total_length() == total_length
                    && control_file.bitfield().len() == CHECKPOINT_PIECES.div_ceil(8)
        );
        if continue_download || has_compatible_checkpoint {
            existing_length
        } else {
            0
        }
    }

    pub(crate) fn disabled(output_path: &Path) -> Self {
        Self {
            control_file: None,
            last_saved_length: 0,
            path: ControlFile::control_path_for(output_path),
        }
    }

    /// Open a compatible sidecar or create one for a known-length download.
    ///
    /// Invalid or stale sidecars are discarded. Checkpoint persistence is
    /// best-effort: a read-only filesystem must not turn a protocol download
    /// into a different wire-level failure.
    pub(crate) async fn open(output_path: &Path, total_length: u64, existing_length: u64) -> Self {
        let path = ControlFile::control_path_for(output_path);
        if total_length == 0 {
            return Self::disabled(output_path);
        }

        let output_exists = output_path.exists();
        let compatible = if !output_exists && path.exists() {
            remove_stale(&path).await;
            None
        } else {
            match ControlFile::load(&path).await {
                Ok(Some(control_file))
                    if control_file.total_length() == total_length
                        && control_file.bitfield().len() == CHECKPOINT_PIECES.div_ceil(8) =>
                {
                    Some(control_file)
                }
                Ok(Some(_)) => {
                    remove_stale(&path).await;
                    None
                }
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(
                        path = %path.display(),
                        %error,
                        "Ignoring unreadable download checkpoint"
                    );
                    remove_stale(&path).await;
                    None
                }
            }
        };

        let mut checkpoint = Self {
            last_saved_length: compatible
                .as_ref()
                .map(ControlFile::completed_length)
                .unwrap_or(existing_length.min(total_length)),
            control_file: compatible,
            path,
        };

        if let Some(control_file) = checkpoint.control_file.as_mut() {
            let trusted_length = control_file
                .completed_length()
                .min(existing_length)
                .min(total_length);
            if trusted_length != control_file.completed_length() {
                control_file.update_completed_length(trusted_length);
                checkpoint.last_saved_length = trusted_length;
                checkpoint.save().await;
            }
        }

        if checkpoint.control_file.is_none() {
            match ControlFile::open_or_create(&checkpoint.path, total_length, CHECKPOINT_PIECES)
                .await
            {
                Ok(mut control_file) => {
                    control_file.update_completed_length(existing_length.min(total_length));
                    checkpoint.last_saved_length = control_file.completed_length();
                    checkpoint.control_file = Some(control_file);
                    checkpoint.save().await;
                }
                Err(error) => {
                    tracing::debug!(
                        path = %checkpoint.path.display(),
                        %error,
                        "Download checkpoint is unavailable"
                    );
                }
            }
        }

        checkpoint
    }

    pub(crate) fn resume_offset(&self, existing_length: u64) -> u64 {
        self.control_file
            .as_ref()
            .map(ControlFile::completed_length)
            .unwrap_or(existing_length)
            .min(existing_length)
    }

    pub(crate) async fn stored_total_length(output_path: &Path) -> Option<u64> {
        ControlFile::load(&ControlFile::control_path_for(output_path))
            .await
            .ok()
            .flatten()
            .map(|control_file| control_file.total_length())
    }

    /// Save progress when the batch threshold is reached, or immediately
    /// when `force` is used for a cancellation or terminal lifecycle edge.
    pub(crate) async fn update(&mut self, completed_length: u64, force: bool) {
        let Some(control_file) = self.control_file.as_mut() else {
            return;
        };

        let completed_length = completed_length.min(control_file.total_length());
        if !force
            && completed_length.saturating_sub(self.last_saved_length) < CHECKPOINT_SAVE_INTERVAL
        {
            return;
        }

        control_file.update_completed_length(completed_length);
        self.save().await;
    }

    pub(crate) async fn complete(self) {
        if self.control_file.is_none() {
            return;
        }
        if let Err(error) = tokio::fs::remove_file(&self.path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(
                path = %self.path.display(),
                %error,
                "Failed to remove completed download checkpoint"
            );
        }
    }

    /// Discard an invalid attempt and reset its output.
    ///
    /// Protocol adapters use this only after an attempt has reached a
    /// terminal validation failure, such as a size or checksum mismatch.
    /// Cancellation paths deliberately save progress so a paused or removed
    /// download remains resumable.
    pub(crate) async fn discard(self, output_path: &Path) {
        if let Err(error) = tokio::fs::remove_file(&self.path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(
                path = %self.path.display(),
                %error,
                "Failed to remove invalid download checkpoint"
            );
        }

        let file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(output_path)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                tracing::debug!(
                    path = %output_path.display(),
                    %error,
                    "Failed to reset invalid download output"
                );
                return;
            }
        };
        if let Err(error) = file.sync_data().await {
            tracing::debug!(
                path = %output_path.display(),
                %error,
                "Failed to flush reset download output"
            );
        }
    }

    async fn save(&mut self) {
        let Some(control_file) = self.control_file.as_ref() else {
            return;
        };
        if let Err(error) = control_file.save().await {
            tracing::debug!(
                path = %control_file.path().display(),
                %error,
                "Failed to save download checkpoint"
            );
            return;
        }
        self.last_saved_length = control_file.completed_length();
    }
}

async fn remove_stale(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(
            path = %path.display(),
            %error,
            "Failed to remove stale download checkpoint"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compatible_checkpoint_restores_only_existing_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("download.bin");
        tokio::fs::write(&output, vec![0u8; 400]).await.unwrap();

        let mut checkpoint = ProgressCheckpoint::open(&output, 1000, 400).await;
        assert_eq!(checkpoint.resume_offset(400), 400);
        checkpoint.update(400, true).await;
        assert!(ControlFile::control_path_for(&output).exists());
    }

    #[tokio::test]
    async fn stale_checkpoint_is_replaced_and_completion_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("download.bin");
        tokio::fs::write(&output, vec![0u8; 200]).await.unwrap();
        let path = ControlFile::control_path_for(&output);

        let stale = ControlFile::open_or_create(&path, 2000, CHECKPOINT_PIECES)
            .await
            .unwrap();
        stale.save().await.unwrap();

        let checkpoint = ProgressCheckpoint::open(&output, 1000, 200).await;
        assert_eq!(checkpoint.resume_offset(200), 200);
        assert_eq!(
            ControlFile::load(&path)
                .await
                .unwrap()
                .unwrap()
                .total_length(),
            1000
        );

        checkpoint.complete().await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn checkpoint_does_not_create_sidecar_for_zero_length_download() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("empty.bin");
        let checkpoint = ProgressCheckpoint::open(&output, 0, 0).await;
        checkpoint.complete().await;
        assert!(!ControlFile::control_path_for(&output).exists());
    }

    #[tokio::test]
    async fn continue_false_does_not_reuse_output_without_valid_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("fresh.bin");
        tokio::fs::write(&output, vec![0u8; 400]).await.unwrap();

        assert_eq!(
            ProgressCheckpoint::resume_input_length(&output, 400, false, 1000).await,
            0
        );

        let mut checkpoint = ControlFile::open_or_create(
            &ControlFile::control_path_for(&output),
            1000,
            CHECKPOINT_PIECES,
        )
        .await
        .unwrap();
        checkpoint.update_completed_length(200);
        checkpoint.save().await.unwrap();

        assert_eq!(
            ProgressCheckpoint::resume_input_length(&output, 400, false, 1000).await,
            400
        );
    }
}
