use std::collections::HashMap;
use std::time::Instant;

use crate::engine::work_scheduler::{WorkId, WorkItem, WorkLease, WorkScheduler};
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use super::PieceDownloadSession;

pub(super) const MAX_WEB_SEED_PIECES_IN_FLIGHT: usize = 4;

fn scheduler_error(context: &str, error: impl std::fmt::Display) -> Aria2Error {
    Aria2Error::Fatal(FatalError::Config(format!(
        "WebSeed work scheduler could not {context}: {error}"
    )))
}

pub(super) async fn wait_for_uri_generation(
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    notifier: std::sync::Arc<tokio::sync::Notify>,
    observed: u64,
) {
    loop {
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if generation.load(std::sync::atomic::Ordering::Acquire) != observed {
            return;
        }
        notified.await;
    }
}

impl PieceDownloadSession<'_> {
    pub(super) fn schedule_web_seed_pieces(
        &mut self,
        tasks: &mut tokio::task::JoinSet<(u32, std::result::Result<Vec<u8>, String>)>,
        active_work: &mut HashMap<u32, WorkLease<u32>>,
        scan_cursor: &mut u32,
        scheduler: &mut WorkScheduler<u32>,
        concurrency: usize,
    ) -> Result<()> {
        let Some(manager) = self.web_seed_manager.as_ref() else {
            return Ok(());
        };
        while tasks.len() < concurrency {
            let lease = scheduler.admit_one_where(concurrency, Instant::now(), |piece_index| {
                let piece_data_length = self.actual_piece_length(*piece_index as usize);
                !active_work.contains_key(piece_index)
                    && self.piece_picker.is_allowed(*piece_index)
                    && !self.piece_picker.is_completed(*piece_index)
                    && !self.piece_picker.is_in_progress(*piece_index)
                    && !self.piece_picker.is_reserved(*piece_index)
                    && piece_data_length > 0
                    && manager.has_complete_sources_for_piece(*piece_index, piece_data_length)
            });
            if let Some(lease) = lease {
                let piece_index = *lease.payload();
                self.piece_picker.mark_reserved(piece_index, true);
                active_work.insert(piece_index, lease);
                let manager = std::sync::Arc::clone(manager);
                let group = std::sync::Arc::clone(&self.command.group);
                let progress = std::sync::Arc::clone(&self.command.progress);
                let piece_data_length = self.actual_piece_length(piece_index as usize);
                tasks.spawn(async move {
                    let connection = ActiveConnectionGuard::new(group);
                    connection.set(1);
                    let result = manager
                        .request_piece_with_length_and_activity(
                            piece_index,
                            piece_data_length as u64,
                            Some(progress.as_ref()),
                        )
                        .await;
                    (piece_index, result)
                });
                continue;
            }

            if *scan_cursor >= self.num_pieces {
                break;
            }
            let piece_index = *scan_cursor;
            *scan_cursor += 1;
            if !self.piece_picker.is_allowed(piece_index)
                || self.piece_picker.is_completed(piece_index)
                || self.piece_picker.is_in_progress(piece_index)
                || self.piece_picker.is_reserved(piece_index)
            {
                continue;
            }
            let piece_data_length = self.actual_piece_length(piece_index as usize);
            if piece_data_length == 0
                || !manager.has_complete_sources_for_piece(piece_index, piece_data_length)
            {
                continue;
            }
            let id = WorkId::new(u64::from(piece_index));
            if scheduler.is_scheduled(id) {
                continue;
            }
            let max_attempts = self
                .command
                .group
                .recover()
                .options()
                .max_retries
                .saturating_add(1);
            scheduler
                .enqueue(WorkItem::new(id, piece_index, max_attempts))
                .map_err(|error| scheduler_error("queue a piece", error))?;
        }
        Ok(())
    }
}
