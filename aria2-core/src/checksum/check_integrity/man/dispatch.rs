use std::sync::{Arc, OnceLock, atomic::Ordering};
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::error::Result;

use super::cancelled_error;
use super::queue::{CheckIntegrityEntry, CheckIntegrityMan};
use super::tasks::IntegrityOutcome;

// Shared instance + worker loop
// ---------------------------------------------------------------------------

/// Thread-safe wrapper for engine integration.
pub type SharedCheckIntegrityMan = Arc<RwLock<CheckIntegrityMan>>;

/// Process-wide shared check-integrity manager with a lazily spawned worker.
///
/// The worker owns a dedicated single-thread Tokio runtime instead of being
/// spawned on the caller's runtime. Library users and `#[tokio::test]` each
/// commonly create and destroy their own runtimes; binding this process-wide
/// queue to one of them would cancel the worker while other users still hold
/// the shared manager.
pub fn shared() -> SharedCheckIntegrityMan {
    static SHARED: OnceLock<SharedCheckIntegrityMan> = OnceLock::new();
    static WORKER: OnceLock<()> = OnceLock::new();

    let man = SHARED
        .get_or_init(|| Arc::new(RwLock::new(CheckIntegrityMan::new())))
        .clone();
    let worker_manager = Arc::clone(&man);
    WORKER.get_or_init(move || spawn_worker(worker_manager, false));
    man
}

/// Shared manager with the given concurrency (mainly for tests; spawns its
/// own worker on a runtime independent from the caller.
pub fn shared_with_concurrency(max: usize) -> SharedCheckIntegrityMan {
    let man = Arc::new(RwLock::new(CheckIntegrityMan::with_concurrency(max)));
    spawn_worker(Arc::clone(&man), true);
    man
}

fn spawn_worker(man: SharedCheckIntegrityMan, stop_when_unowned: bool) {
    std::thread::Builder::new()
        .name("aria2-check-integrity".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("check-integrity worker runtime must initialize");
            runtime.block_on(worker_loop(man, stop_when_unowned));
        })
        .expect("check-integrity worker thread must start");
}

/// Background worker: pick queued entries, drive validation chunk-by-chunk,
/// then notify the waiter with the validation outcome.
async fn worker_loop(man: SharedCheckIntegrityMan, stop_when_unowned: bool) {
    debug!("Check integrity worker started");

    loop {
        let wake = {
            let guard = man.read().await;
            guard.wake_notifier()
        };
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let entry = {
            let mut guard = man.write().await;
            guard.take_next_owned()
        };

        let Some(mut entry) = entry else {
            // Queue insertion and cancellation both notify this waiter. The
            if stop_when_unowned {
                // The short timeout only lets isolated test managers notice
                // that the caller released its last reference. The
                // process-wide singleton remains event-driven and alive
                // through its static manager reference.
                if tokio::time::timeout(std::time::Duration::from_millis(100), notified)
                    .await
                    .is_err()
                    && Arc::strong_count(&man) == 1
                {
                    debug!("Check integrity worker stopped after manager release");
                    break;
                }
            } else {
                notified.await;
            }
            continue;
        };

        let result = run_validation(&mut entry).await;

        let mut guard = man.write().await;
        guard.drop_picked();
        drop(guard);
        if let Some(tx) = entry.take_done_sender() {
            let _ = tx.send(result);
        }
    }
}

/// Drive one entry to completion.
async fn run_validation(entry: &mut CheckIntegrityEntry) -> Result<IntegrityOutcome> {
    let started = Instant::now();
    let gid = entry.gid;

    if entry.is_cancelled() {
        return Err(cancelled_error());
    }

    let result: Result<IntegrityOutcome> = async {
        while !entry.task.is_finished() {
            if entry.is_cancelled() {
                return Err(cancelled_error());
            }
            entry.task.validate_chunk().await?;
            entry.progress().store(
                entry.task.current_length().min(entry.task.total_length()),
                Ordering::Relaxed,
            );
            // Cooperative scheduling: never hog a worker thread on big files.
            tokio::task::yield_now().await;
        }
        Ok(IntegrityOutcome {
            verified: entry.task.passed(),
            failed_piece_indices: entry.task.failed_piece_indices(),
            verified_piece_indices: entry.task.verified_piece_indices(),
        })
    }
    .await;

    match &result {
        Ok(outcome) if outcome.verified => info!(
            gid,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "Integrity check passed"
        ),
        Ok(_) => warn!(
            gid,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "Integrity check failed (piece mismatch)"
        ),
        Err(e) => warn!(gid, error = %e, "Integrity check failed"),
    }
    result
}

// ---------------------------------------------------------------------------
