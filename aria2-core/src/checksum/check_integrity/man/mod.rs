//! Check integrity manager: sequential queue + background worker for chunked
//! file integrity validation.
//!
//! Mirrors C++ `CheckIntegrityMan` (a `SequentialPicker<CheckIntegrityEntry>`)
//! plus `CheckIntegrityDispatcherCommand` + `CheckIntegrityCommand` that drive
//! chunked validation inside the event loop.
//!
//! # C++ Architecture
//!
//! 1. `CheckIntegrityMan` = `SequentialPicker<CheckIntegrityEntry>` — a queue
//!    that picks one entry at a time (sequential, not concurrent).
//! 2. `CheckIntegrityEntry` holds an `IteratableValidator` (piece-hash or
//!    whole-file) plus the post-check actions (`onDownloadFinished` /
//!    `onDownloadIncomplete`).
//! 3. `CheckIntegrityDispatcherCommand` (realtime, periodic) picks the next
//!    queued entry and hands it to a `CheckIntegrityCommand`.
//! 4. `CheckIntegrityCommand` calls `validateChunk()` once per event-loop tick
//!    so validating a large file never blocks the loop; when `finished()` it
//!    branches on the download being complete (→ allocation/download) or not
//!    (→ re-download), then drops the picked entry.
//!
//! # Rust Design
//!
//! The same semantics are provided with a background tokio task:
//! - `CheckIntegrityMan` holds a `VecDeque` of pending entries plus the active
//!   one; `max_concurrent` defaults to 1 (C++-matching sequential dispatch).
//! - A single worker loop takes the next entry, drives the
//!   [`CheckIntegrityTask`] chunk-by-chunk with `yield_now()` between chunks
//!   (async equivalent of per-tick `validateChunk()`), then signals the
//!   outcome (`Ok(true)` = verified, `Ok(false)` = mismatched, `Err` =
//!   I/O failure or cancellation) through a `oneshot` channel.
//! - [`enqueue`] waits for the outcome, so the calling download command
//!   resumes exactly when validation is done (mirroring C++ where the next
//!   commands are created only after the check completes).
//! - [`cancel_all`] clears the queue and notifies waiters so an engine halt
//!   never leaves a command hanging on a check that will not run.

use crate::error::Aria2Error;

mod dispatch;
mod helpers;
mod queue;
mod tasks;

#[cfg(test)]
mod tests;

pub use dispatch::{SharedCheckIntegrityMan, shared, shared_with_concurrency};
pub use helpers::{
    cancel_all, cancel_gid, cut_multi_file_trailing_garbage, cut_trailing_garbage, enqueue,
    enqueue_file_checksum_for_group, enqueue_with_outcome, enqueue_with_outcome_for_group,
    file_task, multi_file_task,
};
pub use queue::{CheckIntegrityEntry, CheckIntegrityMan};
pub use tasks::{
    CheckIntegrityTask, FileChecksumTask, FileChunkValidator, IntegrityOutcome,
    MultiFileChunkValidator,
};

/// Error reported when a queued integrity check is cancelled (engine halt).
pub(super) fn cancelled_error() -> Aria2Error {
    Aria2Error::DownloadFailed("integrity check cancelled".to_string())
}
