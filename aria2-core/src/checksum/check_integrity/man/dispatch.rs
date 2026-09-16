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
/// Must be called from a tokio runtime context.
pub fn shared() -> SharedCheckIntegrityMan {
    static SHARED: OnceLock<SharedCheckIntegrityMan> = OnceLock::new();
    static WORKER: OnceLock<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        OnceLock::new();

    let man = SHARED
        .get_or_init(|| Arc::new(RwLock::new(CheckIntegrityMan::new())))
        .clone();
    // A worker is tied to the runtime that spawned it. Recreate it when the
    // previous runtime has shut down instead of retaining a stale start flag.
    let worker = WORKER.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = match worker.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard
        .as_ref()
        .is_none_or(tokio::task::JoinHandle::is_finished)
    {
        *guard = Some(tokio::spawn(worker_loop(man.clone())));
    }
    man
}

/// Shared manager with the given concurrency (mainly for tests; spawns its
/// own worker).
pub fn shared_with_concurrency(max: usize) -> SharedCheckIntegrityMan {
    let man = Arc::new(RwLock::new(CheckIntegrityMan::with_concurrency(max)));
    tokio::spawn(worker_loop(man.clone()));
    man
}

/// Background worker: pick queued entries, drive validation chunk-by-chunk,
/// then notify the waiter with the validation outcome.
async fn worker_loop(man: SharedCheckIntegrityMan) {
    debug!("Check integrity worker started");
    let wake = {
        let guard = man.read().await;
        guard.wake_notifier()
    };

    loop {
        let entry = {
            let mut guard = man.write().await;
            guard.take_next_owned()
        };

        let Some(mut entry) = entry else {
            // Queue insertion and cancellation both notify this waiter. The
            // worker therefore consumes no timer wakeups while idle.
            wake.notified().await;
            continue;
        };

        let result = run_validation(&mut entry).await;

        {
            let mut guard = man.write().await;
            guard.drop_picked();
        }
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
