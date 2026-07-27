//! Engine command types for the download engine's internal communication.
//!
//! `EngineCommand` is the message type sent through the engine's command channel.
//! RPC handlers and the CLI submit commands by sending `EngineCommand` variants;
//! the engine loop processes them in order.
//!
//! `TaskResult` is sent back by spawned download tasks when they complete,
//! allowing the engine to track group lifecycle (decrement `num_commands`,
//! check for demotion to stopped).

use std::sync::Arc;

use crate::error::Aria2Error;
use crate::request::request_group::{GroupId, HaltReason, RequestGroup};

/// Commands sent to the engine loop via the command channel.
///
/// This replaces the previous `Box<dyn Command>` channel with typed variants
/// that the engine can dispatch without downcasting. Download commands are
/// created from promoted groups during `fill_from_reserver()`.
pub enum EngineCommand {
    /// Add a new download group to the reserved queue.
    /// The engine will promote it to active when a slot is available.
    AddDownload {
        group: Arc<std::sync::RwLock<RequestGroup>>,
    },

    /// Remove a download group by GID (from either active or reserved).
    RemoveDownload { gid: GroupId },

    /// Pause an active or reserved download.
    Pause { gid: GroupId },

    /// Force-pause an active download (abort in-flight commands).
    ForcePause { gid: GroupId },

    /// Unpause a paused download (moves it back to waiting for promotion).
    Unpause { gid: GroupId },

    /// A spawned download task completed (successfully or with error).
    /// The engine uses this to decrement `num_commands` and check for demotion.
    TaskCompleted { gid: GroupId, result: TaskResult },

    /// Pause all active and reserved downloads.
    PauseAll,

    /// Force-pause all active and reserved downloads.
    ForcePauseAll,

    /// Unpause all paused downloads.
    UnpauseAll,

    /// Request graceful halt of all downloads (let in-flight chunks finish).
    HaltAll { reason: HaltReason },

    /// Request forced halt of all downloads (abort immediately).
    ForceHaltAll { reason: HaltReason },

    /// Change the maximum concurrent download limit.
    SetMaxConcurrent { max: u32 },
}

/// Result of a completed download task, sent back to the engine loop.
#[derive(Debug)]
pub enum TaskResult {
    /// Download completed successfully.
    Success,

    /// Download failed with an error.
    Failed(Aria2Error),

    /// Download was cancelled (halt/pause requested).
    Cancelled,
}
