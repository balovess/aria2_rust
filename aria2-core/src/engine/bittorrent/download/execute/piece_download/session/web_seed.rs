use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use super::PieceDownloadSession;

pub(super) const MAX_WEB_SEED_PIECES_IN_FLIGHT: usize = 4;

#[derive(Default)]
pub(super) struct WebSeedRetries {
    due: BinaryHeap<Reverse<(Instant, u32)>>,
    attempts: HashMap<u32, u32>,
}

impl WebSeedRetries {
    pub(super) fn schedule(&mut self, piece_index: u32, max_retries: u32, retry_wait: Duration) {
        let attempts = self.attempts.entry(piece_index).or_default();
        if *attempts >= max_retries {
            return;
        }
        *attempts += 1;
        let now = Instant::now();
        let deadline = now.checked_add(retry_wait).unwrap_or(now);
        self.due.push(Reverse((deadline, piece_index)));
    }

    pub(super) fn pop_ready(&mut self, now: Instant) -> Option<u32> {
        self.due
            .peek()
            .is_some_and(|Reverse((deadline, _))| *deadline <= now)
            .then(|| self.due.pop().map(|Reverse((_, piece_index))| piece_index))
            .flatten()
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.due.peek().map(|Reverse((deadline, _))| *deadline)
    }

    pub(super) fn clear(&mut self) {
        self.due.clear();
        self.attempts.clear();
    }
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
        active_pieces: &mut HashSet<u32>,
        scan_cursor: &mut u32,
        retries: &mut WebSeedRetries,
        concurrency: usize,
    ) {
        let Some(manager) = self.web_seed_manager.as_ref() else {
            return;
        };
        while tasks.len() < concurrency {
            let piece_index = match retries.pop_ready(Instant::now()) {
                Some(piece_index) => piece_index,
                None if *scan_cursor < self.num_pieces => {
                    let piece_index = *scan_cursor;
                    *scan_cursor += 1;
                    piece_index
                }
                None => break,
            };
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

            self.piece_picker.mark_reserved(piece_index, true);
            active_pieces.insert(piece_index);
            let manager = std::sync::Arc::clone(manager);
            let group = std::sync::Arc::clone(&self.command.group);
            let progress = std::sync::Arc::clone(&self.command.progress);
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
        }
    }

    pub(super) fn schedule_web_seed_retry(&self, piece_index: u32, retries: &mut WebSeedRetries) {
        let group = self.command.group.recover();
        retries.schedule(
            piece_index,
            group.options().max_retries,
            Duration::from_secs(group.options().retry_wait),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::WebSeedRetries;
    use std::time::{Duration, Instant};

    #[test]
    fn web_seed_retries_are_delayed_and_limited() {
        let now = Instant::now();
        let mut delayed = WebSeedRetries::default();
        delayed.schedule(7, 2, Duration::from_secs(60));
        assert_eq!(delayed.pop_ready(now), None);
        assert!(delayed.next_deadline().is_some());

        let mut retries = WebSeedRetries::default();
        retries.schedule(7, 2, Duration::ZERO);
        assert_eq!(retries.pop_ready(Instant::now()), Some(7));

        retries.schedule(7, 2, Duration::ZERO);
        assert_eq!(retries.pop_ready(Instant::now()), Some(7));

        retries.schedule(7, 2, Duration::ZERO);
        assert_eq!(retries.pop_ready(Instant::now()), None);
    }
}
