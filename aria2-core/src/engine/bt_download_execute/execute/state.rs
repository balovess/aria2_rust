use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_piece_selector::build_bitfield_from_completed;
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

use super::checkpoint::initial_bt_progress;

pub(super) struct IntegrityPreparation {
    pub(super) payload_exists: bool,
    pub(super) seed_unverified: bool,
    pub(super) verified_piece_indices: Vec<usize>,
    pub(super) integrity_finished_action: crate::checksum::check_integrity::IntegrityFinishedAction,
}

impl BtDownloadCommand {
    pub(super) async fn prepare_integrity_state(
        &mut self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        network_info_hash: [u8; 20],
    ) -> Result<IntegrityPreparation> {
        let payload_exists = self.bt_payload_exists();
        let checkpoint = crate::engine::bt_checkpoint::BtCheckpoint::open(
            &self.output_path,
            payload_exists,
            total_size,
            piece_length,
            num_pieces as usize,
            network_info_hash,
        )
        .await?;
        let checkpoint_completed_length = checkpoint.completed_length();
        let (command_completed_length, visible_completed_length) =
            initial_bt_progress(self.check_integrity, checkpoint_completed_length);
        self.completed_bytes = command_completed_length;
        // Integrity checking may take a long time for a large payload. Keep
        // the last durable piece progress visible while it runs; the
        // verified-piece result below replaces it if corruption is found.
        self.progress.set_completed_length(visible_completed_length);
        self.group
            .recover()
            .set_bt_bitfield(checkpoint.bitfield().map(ToOwned::to_owned));
        self.checkpoint = Some(checkpoint);
        self.checkpoint_bytes_since_save = 0;
        self.checkpoint_last_save = Instant::now();

        // C++ `--bt-seed-unverified` marks an existing payload complete before
        // the integrity command is scheduled. Keep hash-check-only explicit:
        // it is a diagnostic request and must still validate the payload.
        let seed_unverified = self.bt_seed_unverified && payload_exists && !self.hash_check_only;
        let mut verified_piece_indices = if seed_unverified {
            info!(
                "bt-seed-unverified enabled; treating existing payload as complete without piece-hash validation"
            );
            self.completed_bytes = total_size;
            self.progress.set_completed_length(total_size);
            (0..num_pieces as usize).collect()
        } else {
            Vec::new()
        };

        // --check-integrity: verify existing data against the torrent's piece
        // hashes before allocating/downloading (mirrors C++
        // CheckIntegrityMan + CheckIntegrityCommand).
        let mut integrity_finished_action =
            crate::checksum::check_integrity::IntegrityFinishedAction::default();
        if self.check_integrity && !seed_unverified {
            use crate::checksum::check_integrity::{IntegrityTrailingGarbageAction, man as ci_man};
            use crate::checksum::message_digest::HashType;
            use crate::util::rwlock_ext::RwLockRecover;
            let gid = self.group.recover().gid().value();
            let piece_hashes_hex: Vec<String> = meta.info.pieces.iter().map(hex::encode).collect();
            let integrity_files = self.integrity_files(total_size);
            IntegrityTrailingGarbageAction::new(integrity_files.clone())
                .apply()
                .await?;
            let task = if self.multi_file_layout.is_some() {
                ci_man::multi_file_task(
                    integrity_files
                        .iter()
                        .map(|file| (file.path.clone(), file.length))
                        .collect(),
                    piece_length as u64,
                    total_size,
                    piece_hashes_hex,
                    HashType::Sha1,
                )?
            } else {
                ci_man::file_task(
                    &self.output_path,
                    piece_length as u64,
                    total_size,
                    piece_hashes_hex,
                    HashType::Sha1,
                )?
            };
            if let Some(task) = task {
                info!(
                    gid,
                    "Checking integrity of existing data against piece hashes"
                );
                let outcome = ci_man::enqueue_with_outcome_for_group(
                    &ci_man::shared(),
                    Arc::clone(&self.group),
                    task,
                )
                .await?;
                verified_piece_indices = outcome.verified_piece_indices;
                if !outcome.failed_piece_indices.is_empty() {
                    warn!(
                        gid,
                        failed_pieces = ?outcome.failed_piece_indices,
                        "Integrity check found pieces to re-download"
                    );
                }
                // Only verified pieces enter the picker as complete. Failed
                // pieces are intentionally left missing, which makes the
                // runtime piece manager request them again rather than relying
                // on stale control-file state.
                info!(
                    gid,
                    verified_pieces = verified_piece_indices.len(),
                    "Integrity check completed, proceeding with download"
                );
                integrity_finished_action =
                    crate::checksum::check_integrity::IntegrityFinishedAction::for_bt(
                        integrity_files,
                        self.hash_check_only,
                        self.bt_hash_check_seed,
                        self.bt_enable_hook_after_hash_check,
                    );
                self.completed_bytes = verified_piece_indices
                    .iter()
                    .filter_map(|&index| {
                        (index < num_pieces as usize).then_some(
                            total_size
                                .saturating_sub(index as u64 * piece_length as u64)
                                .min(piece_length as u64),
                        )
                    })
                    .sum();
                self.progress.set_completed_length(self.completed_bytes);
                if let Some(checkpoint) = self.checkpoint.as_mut()
                    && let Err(error) = checkpoint
                        .save_verified_pieces(
                            verified_piece_indices.iter().copied(),
                            self.completed_bytes,
                        )
                        .await
                {
                    warn!(%error, "Failed to rewrite BT checkpoint after integrity checking");
                }

                // A complete integrity check is a distinct lifecycle from a
                // normal piece download. The original public contract emits
                // the BT completion hook at this seam and only continues
                // into peer/seed setup when bt-hash-check-seed is enabled.
                if verified_piece_indices.len() == num_pieces as usize && !self.hash_check_only {
                    self.completed_bytes = total_size;
                    self.progress.set_completed_length(total_size);
                    self.hash_check_completed = true;
                    self.bt_complete_event_emitted = true;
                    if integrity_finished_action.run_completion_hook {
                        crate::engine::download_event_hooks::DownloadEventHooks::shared()
                            .fire_event(
                                crate::engine::download_event_hooks::DownloadEvent::BtComplete,
                                &self.group.recover(),
                            );
                    }
                }
            }
        }

        // Integrity results and the explicit unverified-seed path both replace
        // the checkpoint's trust state. Publish the same verified-piece view
        // to RPC, CLI, and TUI before the command enters peer/seeding phases.
        if self.check_integrity || seed_unverified {
            let verified_piece_set: HashSet<usize> =
                verified_piece_indices.iter().copied().collect();
            let verified_bitfield = build_bitfield_from_completed(num_pieces, |index| {
                verified_piece_set.contains(&(index as usize))
            });
            self.group
                .recover()
                .set_bt_bitfield(Some(verified_bitfield));
        }
        Ok(IntegrityPreparation {
            payload_exists,
            seed_unverified,
            verified_piece_indices,
            integrity_finished_action,
        })
    }
}
