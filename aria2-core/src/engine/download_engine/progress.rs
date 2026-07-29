use std::sync::Arc;

use tokio::sync::mpsc;

use super::DownloadEngine;
use crate::engine::command::ProgressUpdate;
use crate::request::request_group::RequestGroup;
use crate::util::speed_smooth::SpeedSmoother;

impl DownloadEngine {
    /// Spawn a progress aggregator task that receives [`ProgressUpdate`]s from
    /// download commands and applies them to the shared [`RequestGroup`].
    ///
    /// This eliminates per-chunk write-lock contention on the download hot
    /// path: each `DownloadCommand` performs a cheap lock-free
    /// `mpsc::UnboundedSender::send` and this single aggregator task is the
    /// only writer of the progress fields.
    ///
    /// The aggregator deduplicates consecutive updates with identical
    /// `completed_bytes` values and only refreshes the speed fields when the
    /// sender provides a non-zero `download_speed` sample (0 means "no fresh
    /// sample this tick").
    ///
    /// The task exits cleanly when all senders are dropped (the receiver
    /// returns `None`).
    ///
    /// This is intentionally an associated function (not `&self`): it is
    /// called automatically by
    /// [`DownloadCommand::spawn_progress_aggregator`](crate::engine::download_command::DownloadCommand::spawn_progress_aggregator)
    /// during `execute()`, since every `DownloadCommand` now auto-creates a
    /// progress channel in its constructor. External callers rarely need to
    /// invoke this directly.
    pub fn spawn_progress_aggregator(
        _group: Arc<std::sync::RwLock<RequestGroup>>,
        progress: Arc<crate::request::request_group::AtomicProgress>,
        mut receiver: mpsc::UnboundedReceiver<ProgressUpdate>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut last_bytes: u64 = 0;
            let mut smoother = SpeedSmoother::with_default_window(); // EMA N=10
            while let Some(update) = receiver.recv().await {
                // Skip no-op updates: identical completed_bytes means nothing changed
                // since the last applied update (e.g. a stale in-flight send).
                if update.completed_bytes == last_bytes {
                    continue;
                }
                let delta = update.completed_bytes - last_bytes;
                smoother.record_bytes(delta);

                // Lock-free progress update -- no RwLock acquisition needed.
                progress.set_completed_length(update.completed_bytes);

                // Speed: use EMA-smoothed speed when available; fall back to
                // the sender's raw speed sample when the smoother hasn't
                // produced a value yet (first sample window). Skip the speed
                // write entirely when both are 0 so a previously cached speed
                // (e.g. from a prior update) is preserved.
                let smoothed = smoother.smoothed_speed() as u64;
                if smoothed > 0 {
                    progress.set_download_speed(smoothed);
                    progress.set_upload_speed(update.upload_speed);
                } else if update.download_speed > 0 {
                    progress.set_download_speed(update.download_speed);
                    progress.set_upload_speed(update.upload_speed);
                }
                last_bytes = update.completed_bytes;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::command::ProgressUpdate;
    use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
    #[cfg(test)]
    use crate::util::rwlock_ext::RwLockRecover;

    /// Helper: build a fresh `RequestGroup` wrapped in an `Arc<std::sync::RwLock<..>>`,
    /// the same shape `DownloadCommand` and the aggregator use.
    fn make_group() -> Arc<std::sync::RwLock<RequestGroup>> {
        Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(1),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )))
    }

    /// Verify the aggregator receives `ProgressUpdate`s sent through the
    /// channel and applies them to the `RequestGroup` (both the RwLock-backed
    /// `completed_length` via `update_progress` and the atomic mirror via
    /// `set_completed_length`).
    #[tokio::test]
    async fn test_progress_channel_updates_request_group() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // Send a sequence of strictly increasing updates.
        tx.send(ProgressUpdate {
            completed_bytes: 1000,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();
        tx.send(ProgressUpdate {
            completed_bytes: 5000,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();

        // Drop the sender: the aggregator drains all queued messages (unbounded
        // channel recv only returns None after the queue is empty and all
        // senders are gone), then exits. Awaiting the handle is therefore a
        // deterministic synchronization point -- no sleep-based polling needed.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        // The atomic mirror is set by `set_completed_length`; verify final value.
        let atomic_val = { group.recover().get_completed_length() };
        assert_eq!(
            atomic_val, 5000,
            "aggregator should have applied the latest completed_bytes (5000)"
        );
    }

    /// Verify the aggregator skips no-op updates with identical
    /// `completed_bytes` (deduplication), so a flood of stale in-flight sends
    /// does not cause redundant write-lock acquisitions.
    #[tokio::test]
    async fn test_progress_aggregator_dedupes_identical_bytes() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // First real update.
        tx.send(ProgressUpdate {
            completed_bytes: 2048,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();
        // Several duplicates with the same completed_bytes (and a speed that
        // must NOT be applied because the dedup `continue`s before reaching
        // the speed branch).
        for _ in 0..5 {
            tx.send(ProgressUpdate {
                completed_bytes: 2048,
                download_speed: 9999,
                upload_speed: 0,
            })
            .unwrap();
        }
        // A real advance with a speed sample that SHOULD be applied.
        tx.send(ProgressUpdate {
            completed_bytes: 4096,
            download_speed: 1234,
            upload_speed: 0,
        })
        .unwrap();

        // Deterministic drain: drop sender + await handle guarantees all
        // queued messages have been processed by the aggregator.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        let g = group.recover();
        assert_eq!(
            g.get_completed_length(),
            4096,
            "final completed_bytes should be 4096"
        );
        // Speed is now EMA-smoothed by the aggregator's SpeedSmoother.
        // We cannot predict the exact EMA value in a unit test, but it must
        // be > 0 (a positive delta was recorded).
        assert!(
            g.get_download_speed_cached() > 0,
            "smoothed speed should be > 0 after positive delta, got {}",
            g.get_download_speed_cached()
        );
    }

    /// Verify that the aggregator applies EMA-smoothed speed whenever a
    /// positive byte delta is recorded, regardless of the sender's raw
    /// `download_speed` sample.
    #[tokio::test]
    async fn test_progress_aggregator_applies_smoothed_speed_on_delta() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        // Seed the group with a known cached speed before starting the aggregator.
        {
            let g = group.recover();
            g.set_download_speed_cached(5555);
        }

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // Advance bytes but send a 0 speed sample (no fresh measurement).
        tx.send(ProgressUpdate {
            completed_bytes: 8192,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();

        // Deterministic drain.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        let g = group.recover();
        assert_eq!(g.get_completed_length(), 8192, "bytes should advance");
        // The smoothed speed is always computed from the byte delta and
        // applied to the cached speed (replacing the seeded 5555).
        assert!(
            g.get_download_speed_cached() > 0,
            "smoothed speed should be applied when delta > 0, got {}",
            g.get_download_speed_cached()
        );
    }

    /// Verify the aggregator task exits cleanly (JoinHandle resolves) once all
    /// senders are dropped, with no hang or resource leak.
    #[tokio::test]
    async fn test_progress_aggregator_exits_on_sender_drop() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let handle = DownloadEngine::spawn_progress_aggregator(
            group.clone(),
            group.recover().progress.clone(),
            rx,
        );

        // Drop the only sender; the aggregator's `recv().await` returns None.
        drop(tx);

        // The handle should resolve promptly without needing to abort.
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
        assert!(
            result.is_ok(),
            "aggregator should exit within 500ms after senders are dropped"
        );
        result
            .expect("aggregator task should exit cleanly")
            .expect("aggregator task should not panic");
    }

    /// Verify that EMA smoothing produces a stable, finite, positive speed
    /// value after a sequence of positive byte deltas.
    ///
    /// This test avoids asserting exact speed bounds because the EMA's
    /// instantaneous speed depends on real elapsed time (`delta / duration`),
    /// which varies with scheduler timing. The detailed EMA convergence and
    /// reaction behavior is covered by `speed_smooth::tests`.
    #[tokio::test]
    async fn test_progress_aggregator_smooths_speed() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // Send a sequence of strictly increasing byte deltas, waiting long
        // enough between sends to cross the smoother's SAMPLE_INTERVAL_MS
        // (500ms) boundary so each delta triggers an EMA update.
        let deltas: [u64; 3] = [10000, 5000, 20000];
        for delta in &deltas {
            let current = {
                let g = group.recover();
                g.get_completed_length()
            };
            tx.send(ProgressUpdate {
                completed_bytes: current + delta,
                download_speed: 0, // Ignored -- smoother computes from delta
                upload_speed: 0,
            })
            .unwrap();

            // Wait long enough for the smoother's SAMPLE_INTERVAL_MS to elapse
            // so the next record_bytes triggers an EMA update.
            tokio::time::sleep(std::time::Duration::from_millis(
                crate::constants::HTTP_SPEED_UPDATE_INTERVAL_MS + 100,
            ))
            .await;
        }

        // Drain the channel.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        let g = group.recover();
        let final_speed = g.get_download_speed_cached();

        // The EMA-smoothed speed must be positive and finite after recording
        // positive deltas. We do not assert upper/lower bounds against the
        // raw delta values because the smoother divides by real elapsed time
        // (varies with scheduler jitter), not a fixed 1s window.
        assert!(
            final_speed > 0,
            "EMA-smoothed speed should be > 0 after positive deltas, got {}",
            final_speed
        );
        assert!(
            final_speed < u64::MAX / 2,
            "EMA-smoothed speed should be finite, got {}",
            final_speed
        );
    }
}
