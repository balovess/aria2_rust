//! Engine command types for the download engine's internal communication.
//!
//! `EngineCommand` is the message type sent through the engine's command channel.
//! RPC handlers and the CLI submit commands by sending `EngineCommand` variants;
//! the engine loop processes them in order.
//!
//! `TaskResult` is sent back by spawned download tasks when they complete,
//! allowing the engine to track group lifecycle (decrement `num_commands`,
//! check for demotion to stopped).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::engine::metalink_request_graph::MetalinkRequestGraph;
use crate::error::Aria2Error;
use crate::request::request_group::{GroupId, HaltReason, RequestGroup};

/// Normal submissions are bounded so an RPC producer cannot grow memory
/// without limit. Control traffic has a separate queue and is drained first.
pub const ENGINE_COMMAND_CAPACITY: usize = 1024;
// Lifecycle bursts (pause/unpause/remove) are control traffic and must not
// be blocked by normal AddDownload submissions. Keep this queue bounded while
// leaving enough headroom for a concurrent RPC batch to drain in one tick.
pub const ENGINE_CONTROL_CAPACITY: usize = 512;
pub const ENGINE_TOTAL_COMMAND_CAPACITY: usize = ENGINE_COMMAND_CAPACITY + ENGINE_CONTROL_CAPACITY;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineCommandQueueSnapshot {
    /// Current depth across the normal and high-priority control queues.
    pub depth: usize,
    /// Maximum observed aggregate depth since channel creation.
    pub max_depth: usize,
    /// Empty-to-non-empty transitions of the aggregate queue. This is the
    /// queue wakeup-edge metric, not an operating-system scheduler count.
    pub wakeups: u64,
    pub dispatch_samples: u64,
    pub dispatch_latency_us_total: u64,
    pub dispatch_latency_us_max: u64,
}

#[derive(Debug, Default)]
struct EngineCommandQueueMetrics {
    depth: AtomicUsize,
    max_depth: AtomicUsize,
    wakeups: AtomicU64,
    dispatch_samples: AtomicU64,
    dispatch_latency_us_total: AtomicU64,
    dispatch_latency_us_max: AtomicU64,
}

impl EngineCommandQueueMetrics {
    fn enqueued(&self) {
        let previous_depth = self.depth.fetch_add(1, Ordering::Relaxed);
        if previous_depth == 0 {
            // A sender only wakes the engine when the aggregate command
            // queues transition from empty to non-empty. Multiple producers
            // joining an already queued batch do not create extra wakeups.
            self.wakeups.fetch_add(1, Ordering::Relaxed);
        }
        let depth = previous_depth + 1;
        let mut max_depth = self.max_depth.load(Ordering::Relaxed);
        while depth > max_depth {
            match self.max_depth.compare_exchange_weak(
                max_depth,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => max_depth = observed,
            }
        }
    }

    fn dequeued(&self, queued_at: Instant) {
        self.depth.fetch_sub(1, Ordering::Relaxed);
        let latency_us = queued_at.elapsed().as_micros().min(u64::MAX as u128) as u64;
        self.dispatch_samples.fetch_add(1, Ordering::Relaxed);
        self.dispatch_latency_us_total
            .fetch_add(latency_us, Ordering::Relaxed);
        let mut max_latency = self.dispatch_latency_us_max.load(Ordering::Relaxed);
        while latency_us > max_latency {
            match self.dispatch_latency_us_max.compare_exchange_weak(
                max_latency,
                latency_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => max_latency = observed,
            }
        }
    }

    fn snapshot(&self) -> EngineCommandQueueSnapshot {
        EngineCommandQueueSnapshot {
            depth: self.depth.load(Ordering::Relaxed),
            max_depth: self.max_depth.load(Ordering::Relaxed),
            wakeups: self.wakeups.load(Ordering::Relaxed),
            dispatch_samples: self.dispatch_samples.load(Ordering::Relaxed),
            dispatch_latency_us_total: self.dispatch_latency_us_total.load(Ordering::Relaxed),
            dispatch_latency_us_max: self.dispatch_latency_us_max.load(Ordering::Relaxed),
        }
    }
}

struct QueuedEngineCommand {
    queued_at: Instant,
    command: EngineCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommandSendError {
    Full,
    Closed,
}

impl std::fmt::Display for EngineCommandSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("engine command queue is full"),
            Self::Closed => f.write_str("engine command queue is closed"),
        }
    }
}

impl std::error::Error for EngineCommandSendError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCommandTryRecvError {
    Empty,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EngineCommandCoalesceKey {
    Remove(GroupId),
    ForceRemove(GroupId),
    Pause(GroupId),
    ForcePause(GroupId),
    Unpause(GroupId),
    PauseAll,
    ForcePauseAll,
    UnpauseAll,
    HaltAll(HaltReason),
    ForceHaltAll(HaltReason),
}

enum EngineCommandSenderBackend {
    Bounded {
        control_tx: mpsc::Sender<QueuedEngineCommand>,
        normal_tx: mpsc::Sender<QueuedEngineCommand>,
        metrics: Arc<EngineCommandQueueMetrics>,
        pending_keys: Arc<Mutex<HashSet<EngineCommandCoalesceKey>>>,
        wake: Arc<Notify>,
    },
    Legacy(mpsc::UnboundedSender<EngineCommand>),
}

/// Cloneable producer for engine commands.
///
/// New engines use bounded normal/control queues. `From<UnboundedSender<_>>`
/// remains available for older embedding fixtures and tests.
pub struct EngineCommandSender {
    backend: Arc<EngineCommandSenderBackend>,
}

impl Clone for EngineCommandSender {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
        }
    }
}

pub struct EngineCommandReceiver {
    backend: EngineCommandReceiverBackend,
}

enum EngineCommandReceiverBackend {
    Bounded {
        control_rx: mpsc::Receiver<QueuedEngineCommand>,
        normal_rx: mpsc::Receiver<QueuedEngineCommand>,
        metrics: Arc<EngineCommandQueueMetrics>,
        pending_keys: Arc<Mutex<HashSet<EngineCommandCoalesceKey>>>,
        wake: Arc<Notify>,
        control_closed: bool,
        normal_closed: bool,
    },
    Legacy(mpsc::UnboundedReceiver<EngineCommand>),
}

pub fn channel() -> (EngineCommandSender, EngineCommandReceiver) {
    let (control_tx, control_rx) = mpsc::channel(ENGINE_CONTROL_CAPACITY);
    let (normal_tx, normal_rx) = mpsc::channel(ENGINE_COMMAND_CAPACITY);
    let metrics = Arc::new(EngineCommandQueueMetrics::default());
    let pending_keys = Arc::new(Mutex::new(HashSet::new()));
    let wake = Arc::new(Notify::new());
    (
        EngineCommandSender {
            backend: Arc::new(EngineCommandSenderBackend::Bounded {
                control_tx,
                normal_tx,
                metrics: Arc::clone(&metrics),
                pending_keys: Arc::clone(&pending_keys),
                wake: Arc::clone(&wake),
            }),
        },
        EngineCommandReceiver {
            backend: EngineCommandReceiverBackend::Bounded {
                control_rx,
                normal_rx,
                metrics,
                pending_keys,
                wake,
                control_closed: false,
                normal_closed: false,
            },
        },
    )
}

impl EngineCommandSender {
    pub fn send(&self, command: EngineCommand) -> Result<(), EngineCommandSendError> {
        match self.backend.as_ref() {
            EngineCommandSenderBackend::Legacy(sender) => sender
                .send(command)
                .map_err(|_| EngineCommandSendError::Closed),
            EngineCommandSenderBackend::Bounded {
                control_tx,
                normal_tx,
                metrics,
                pending_keys,
                wake,
            } => {
                let target = if command.is_control() {
                    control_tx
                } else {
                    normal_tx
                };

                // Lifecycle controls are edge-triggered state requests. Keep
                // one pending request per GID and operation so an RPC burst
                // cannot turn repeated pause/remove calls into queue work.
                let coalesce_key = command.coalesce_key();
                if let Some(key) = coalesce_key {
                    if target.is_closed() {
                        return Err(EngineCommandSendError::Closed);
                    }
                    let mut pending = pending_keys
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if !pending.insert(key) {
                        return Ok(());
                    }

                    // Keep the key guard until the command is committed. This
                    // closes the race where a second producer observes a key
                    // before the first producer has secured a bounded slot.
                    let permit = match target.try_reserve() {
                        Ok(permit) => permit,
                        Err(mpsc::error::TrySendError::Full(())) => {
                            pending.remove(&key);
                            return Err(EngineCommandSendError::Full);
                        }
                        Err(mpsc::error::TrySendError::Closed(())) => {
                            pending.remove(&key);
                            return Err(EngineCommandSendError::Closed);
                        }
                    };
                    metrics.enqueued();
                    permit.send(QueuedEngineCommand {
                        queued_at: Instant::now(),
                        command,
                    });
                    wake.notify_one();
                    return Ok(());
                }

                // Reserve the bounded slot before updating metrics. The permit
                // prevents the receiver from dequeuing the command between the
                // depth increment and the actual send.
                let permit = match target.try_reserve() {
                    Ok(permit) => permit,
                    Err(mpsc::error::TrySendError::Full(())) => {
                        return Err(EngineCommandSendError::Full);
                    }
                    Err(mpsc::error::TrySendError::Closed(())) => {
                        return Err(EngineCommandSendError::Closed);
                    }
                };
                metrics.enqueued();
                permit.send(QueuedEngineCommand {
                    queued_at: Instant::now(),
                    command,
                });
                wake.notify_one();
                Ok(())
            }
        }
    }

    pub async fn closed(&self) {
        match self.backend.as_ref() {
            EngineCommandSenderBackend::Bounded { normal_tx, .. } => normal_tx.closed().await,
            EngineCommandSenderBackend::Legacy(sender) => sender.closed().await,
        }
    }

    pub fn snapshot(&self) -> EngineCommandQueueSnapshot {
        match self.backend.as_ref() {
            EngineCommandSenderBackend::Bounded { metrics, .. } => metrics.snapshot(),
            EngineCommandSenderBackend::Legacy(_) => EngineCommandQueueSnapshot::default(),
        }
    }
}

impl From<mpsc::UnboundedSender<EngineCommand>> for EngineCommandSender {
    fn from(sender: mpsc::UnboundedSender<EngineCommand>) -> Self {
        Self {
            backend: Arc::new(EngineCommandSenderBackend::Legacy(sender)),
        }
    }
}

impl EngineCommandReceiver {
    pub fn from_unbounded(receiver: mpsc::UnboundedReceiver<EngineCommand>) -> Self {
        Self {
            backend: EngineCommandReceiverBackend::Legacy(receiver),
        }
    }

    pub fn try_recv(&mut self) -> Result<EngineCommand, EngineCommandTryRecvError> {
        match &mut self.backend {
            EngineCommandReceiverBackend::Legacy(receiver) => {
                receiver.try_recv().map_err(|error| match error {
                    mpsc::error::TryRecvError::Empty => EngineCommandTryRecvError::Empty,
                    mpsc::error::TryRecvError::Disconnected => EngineCommandTryRecvError::Closed,
                })
            }
            EngineCommandReceiverBackend::Bounded {
                control_rx,
                normal_rx,
                metrics,
                pending_keys,
                control_closed,
                normal_closed,
                ..
            } => {
                let queued = if !*control_closed {
                    match control_rx.try_recv() {
                        Ok(command) => Some(command),
                        Err(mpsc::error::TryRecvError::Empty) => None,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            *control_closed = true;
                            None
                        }
                    }
                } else {
                    None
                };
                let queued = queued.or_else(|| {
                    if *normal_closed {
                        return None;
                    }
                    match normal_rx.try_recv() {
                        Ok(command) => Some(command),
                        Err(mpsc::error::TryRecvError::Empty) => None,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            *normal_closed = true;
                            None
                        }
                    }
                });

                if let Some(queued) = queued {
                    metrics.dequeued(queued.queued_at);
                    let command = queued.command;
                    if let Some(key) = command.coalesce_key() {
                        pending_keys
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&key);
                    }
                    Ok(command)
                } else if *control_closed && *normal_closed {
                    Err(EngineCommandTryRecvError::Closed)
                } else {
                    Err(EngineCommandTryRecvError::Empty)
                }
            }
        }
    }

    /// Wait for the next command without a fixed-interval polling delay.
    ///
    /// Bounded control and normal queues share a notification edge. The
    /// receiver still drains control traffic first through `try_recv`, while
    /// `Notify` keeps the idle engine parked until a producer enqueues work.
    pub async fn recv(&mut self) -> Result<EngineCommand, EngineCommandTryRecvError> {
        if let EngineCommandReceiverBackend::Legacy(receiver) = &mut self.backend {
            return receiver
                .recv()
                .await
                .ok_or(EngineCommandTryRecvError::Closed);
        }

        let wake = match &self.backend {
            EngineCommandReceiverBackend::Bounded { wake, .. } => Arc::clone(wake),
            EngineCommandReceiverBackend::Legacy(_) => unreachable!("legacy receiver handled"),
        };

        loop {
            match self.try_recv() {
                Ok(command) => return Ok(command),
                Err(EngineCommandTryRecvError::Closed) => {
                    return Err(EngineCommandTryRecvError::Closed);
                }
                Err(EngineCommandTryRecvError::Empty) => wake.notified().await,
            }
        }
    }
}

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

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    AddMetalinkGraph { graph: MetalinkRequestGraph },

    /// Gracefully remove a download group by GID.
    RemoveDownload { gid: GroupId },

    /// Forcefully remove a download group by GID.
    ForceRemoveDownload { gid: GroupId },

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

    /// Change the process-wide download and upload rate limits.
    ///
    /// `None` means unlimited. The engine keeps one shared limiter handle so
    /// this update also affects commands that were already spawned.
    SetGlobalRateLimit {
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    },

    /// Replace the process-wide remote public tracker list sources.
    #[cfg(feature = "bittorrent")]
    SetPublicTrackerSources { sources: String },

    /// Change the process-wide public tracker refresh interval.
    #[cfg(feature = "bittorrent")]
    SetPublicTrackerUpdateInterval { seconds: u64 },

    /// Enable or disable the process-wide public tracker catalog.
    #[cfg(feature = "bittorrent")]
    SetPublicTrackersEnabled { enabled: bool },
}

impl EngineCommand {
    fn coalesce_key(&self) -> Option<EngineCommandCoalesceKey> {
        match self {
            Self::RemoveDownload { gid } => Some(EngineCommandCoalesceKey::Remove(*gid)),
            Self::ForceRemoveDownload { gid } => Some(EngineCommandCoalesceKey::ForceRemove(*gid)),
            Self::Pause { gid } => Some(EngineCommandCoalesceKey::Pause(*gid)),
            Self::ForcePause { gid } => Some(EngineCommandCoalesceKey::ForcePause(*gid)),
            Self::Unpause { gid } => Some(EngineCommandCoalesceKey::Unpause(*gid)),
            Self::PauseAll => Some(EngineCommandCoalesceKey::PauseAll),
            Self::ForcePauseAll => Some(EngineCommandCoalesceKey::ForcePauseAll),
            Self::UnpauseAll => Some(EngineCommandCoalesceKey::UnpauseAll),
            Self::HaltAll { reason } => Some(EngineCommandCoalesceKey::HaltAll(*reason)),
            Self::ForceHaltAll { reason } => Some(EngineCommandCoalesceKey::ForceHaltAll(*reason)),
            _ => None,
        }
    }

    fn is_control(&self) -> bool {
        let common = matches!(
            self,
            Self::RemoveDownload { .. }
                | Self::ForceRemoveDownload { .. }
                | Self::Pause { .. }
                | Self::ForcePause { .. }
                | Self::Unpause { .. }
                | Self::PauseAll
                | Self::ForcePauseAll
                | Self::UnpauseAll
                | Self::HaltAll { .. }
                | Self::ForceHaltAll { .. }
                | Self::SetMaxConcurrent { .. }
                | Self::SetGlobalRateLimit { .. }
        );
        #[cfg(feature = "bittorrent")]
        let common = common
            || matches!(
                self,
                Self::SetPublicTrackerSources { .. }
                    | Self::SetPublicTrackerUpdateInterval { .. }
                    | Self::SetPublicTrackersEnabled { .. }
            );
        common
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};

    fn add_command(gid: u64) -> EngineCommand {
        EngineCommand::AddDownload {
            group: Arc::new(std::sync::RwLock::new(RequestGroup::new(
                GroupId::new(gid),
                vec![format!("http://example.com/{gid}.bin")],
                DownloadOptions::default(),
            ))),
        }
    }

    #[test]
    fn control_commands_are_drained_before_normal_commands() {
        let (sender, mut receiver) = channel();
        sender.send(add_command(1)).unwrap();
        sender
            .send(EngineCommand::Pause {
                gid: GroupId::new(1),
            })
            .unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(EngineCommand::Pause { .. })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(EngineCommand::AddDownload { .. })
        ));
    }

    #[test]
    fn duplicate_gid_controls_are_coalesced_until_dequeue() {
        let (sender, mut receiver) = channel();
        let pause = || EngineCommand::Pause {
            gid: GroupId::new(7),
        };

        sender.send(pause()).unwrap();
        sender.send(pause()).unwrap();

        let snapshot = sender.snapshot();
        assert_eq!(snapshot.depth, 1);
        assert_eq!(snapshot.max_depth, 1);
        assert_eq!(snapshot.wakeups, 1);
        assert!(matches!(
            receiver.try_recv(),
            Ok(EngineCommand::Pause { .. })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(EngineCommandTryRecvError::Empty)
        ));

        // Once the first command has been dequeued, a later state edge is
        // accepted again rather than being suppressed forever.
        sender.send(pause()).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(EngineCommand::Pause { .. })
        ));
    }

    #[test]
    fn concurrent_duplicate_controls_have_one_committed_queue_entry() {
        let (sender, mut receiver) = channel();
        let mut producers = Vec::new();
        for _ in 0..32 {
            let sender = sender.clone();
            producers.push(std::thread::spawn(move || {
                sender
                    .send(EngineCommand::Pause {
                        gid: GroupId::new(8),
                    })
                    .unwrap();
            }));
        }
        for producer in producers {
            producer.join().unwrap();
        }

        assert_eq!(sender.snapshot().depth, 1);
        assert!(matches!(
            receiver.try_recv(),
            Ok(EngineCommand::Pause { .. })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(EngineCommandTryRecvError::Empty)
        ));
    }

    #[test]
    fn full_normal_queue_rejects_without_growing_metrics() {
        let (sender, mut receiver) = channel();
        for gid in 1..=ENGINE_COMMAND_CAPACITY as u64 {
            sender.send(add_command(gid)).unwrap();
        }

        let before = sender.snapshot();
        assert_eq!(before.depth, ENGINE_COMMAND_CAPACITY);
        assert_eq!(before.max_depth, ENGINE_COMMAND_CAPACITY);
        assert_eq!(before.wakeups, 1);
        assert_eq!(
            sender.send(add_command(ENGINE_COMMAND_CAPACITY as u64 + 1)),
            Err(EngineCommandSendError::Full)
        );
        assert_eq!(sender.snapshot().depth, ENGINE_COMMAND_CAPACITY);
        assert_eq!(sender.snapshot().max_depth, ENGINE_COMMAND_CAPACITY);
        assert_eq!(sender.snapshot().wakeups, 1);

        // A full normal queue must not block shutdown/control traffic.
        sender
            .send(EngineCommand::ForceHaltAll {
                reason: HaltReason::ShutdownSignal,
            })
            .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(EngineCommand::ForceHaltAll { .. })
        ));
    }

    #[test]
    fn closed_queue_rejects_and_receiver_reports_closed() {
        let (sender, receiver) = channel();
        drop(receiver);

        assert_eq!(
            sender.send(EngineCommand::PauseAll),
            Err(EngineCommandSendError::Closed)
        );
        assert_eq!(sender.snapshot(), EngineCommandQueueSnapshot::default());

        let (sender, mut receiver) = channel();
        drop(sender);
        assert!(matches!(
            receiver.try_recv(),
            Err(EngineCommandTryRecvError::Closed)
        ));
    }
}
