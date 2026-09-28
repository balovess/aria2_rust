//! Torrent-scoped owner for tracker announce state and its protocol deadlines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::engine::bt_download_command::{BtRuntimeState, MAX_PUBLIC_TRACKERS_TO_TRY};
use crate::engine::bt_message_handler::PeerEvent;
use crate::engine::bt_tracker_comm::{AnnounceResult, TrackerAnnouncer};
use crate::request::request_group::AtomicProgress;

enum TrackerActorCommand {
    Completed(oneshot::Sender<()>),
    Stop(oneshot::Sender<TrackerAnnouncer>),
}

struct TrackerActorInner {
    command_tx: mpsc::Sender<TrackerActorCommand>,
    task: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    runtime: Arc<BtRuntimeState>,
}

impl Drop for TrackerActorInner {
    fn drop(&mut self) {
        if let Ok(mut task) = self.task.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

/// Cloneable command handle; the spawned task is the sole owner of
/// `TrackerAnnouncer` while the torrent is active.
#[derive(Clone)]
pub(crate) struct BtTrackerAnnouncerActor {
    inner: Arc<TrackerActorInner>,
}

impl BtTrackerAnnouncerActor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        announcer: TrackerAnnouncer,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
        total_size: u64,
        progress: Arc<AtomicProgress>,
        runtime: Arc<BtRuntimeState>,
        enable_public_trackers: bool,
        peer_event_tx: Option<mpsc::Sender<PeerEvent>>,
    ) -> (
        Self,
        Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    ) {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (started_tx, started_rx) = oneshot::channel();
        let task_runtime = Arc::clone(&runtime);
        let task = tokio::spawn(async move {
            run_tracker_actor(
                announcer,
                command_rx,
                started_tx,
                info_hash,
                peer_id,
                total_size,
                progress,
                task_runtime,
                enable_public_trackers,
                peer_event_tx,
            )
            .await;
        });
        let actor = Self {
            inner: Arc::new(TrackerActorInner {
                command_tx,
                task: Mutex::new(Some(task)),
                stopped: AtomicBool::new(false),
                runtime,
            }),
        };
        let initial_peers = started_rx.await.unwrap_or_default();
        (actor, initial_peers)
    }

    /// Queue the completed event behind any in-flight announce, preserving the
    /// announcer's single-writer state machine and tracker-tier ordering.
    pub(crate) async fn announce_completed(&self) {
        if self.inner.stopped.load(Ordering::Acquire) {
            return;
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .inner
            .command_tx
            .send(TrackerActorCommand::Completed(reply_tx))
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }

    pub(crate) fn set_active_connections(&self, connections: usize) {
        self.inner.runtime.set_connections(connections);
    }

    /// Send the terminal stopped event and return the state machine for public
    /// seeding-manager APIs that historically expose `take_announcer`.
    pub(crate) async fn stop(&self) -> Option<TrackerAnnouncer> {
        if self.inner.stopped.swap(true, Ordering::AcqRel) {
            return None;
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let announcer = if self
            .inner
            .command_tx
            .send(TrackerActorCommand::Stop(reply_tx))
            .await
            .is_ok()
        {
            reply_rx.await.ok()
        } else {
            None
        };
        let task = self
            .inner
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        announcer
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_tracker_actor(
    mut announcer: TrackerAnnouncer,
    mut command_rx: mpsc::Receiver<TrackerActorCommand>,
    started_tx: oneshot::Sender<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>>,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    total_size: u64,
    progress: Arc<AtomicProgress>,
    runtime: Arc<BtRuntimeState>,
    enable_public_trackers: bool,
    peer_event_tx: Option<mpsc::Sender<PeerEvent>>,
) {
    // Subscribe before the initial sync so catalog changes during the first
    // announce remain observable without a periodic catalog polling timer.
    let mut public_tracker_updates = if enable_public_trackers {
        announcer.subscribe_public_tracker_updates()
    } else {
        None
    };
    if enable_public_trackers {
        let added = announcer
            .sync_public_trackers(MAX_PUBLIC_TRACKERS_TO_TRY)
            .await;
        if added > 0 {
            debug!(
                added,
                "Synchronized public trackers before initial announce"
            );
        }
    }

    // Preserve aria2's initial started announce and tier failover semantics.
    let mut initial_peers = Vec::new();
    let mut attempts = 0;
    while announcer.is_announce_ready() && attempts < MAX_PUBLIC_TRACKERS_TO_TRY {
        if let Some(result) = announcer
            .announce(&info_hash, &peer_id, 0, total_size, 0)
            .await
        {
            info!(
                peers = result.peers.len(),
                tracker = %result.tracker_url,
                event = ?result.event,
                interval_secs = result.interval.as_secs(),
                "Initial BitTorrent tracker announce completed"
            );
            initial_peers.extend(to_peer_addrs(result));
            if !initial_peers.is_empty() {
                break;
            }
        }
        attempts += 1;
    }
    let _ = started_tx.send(initial_peers);

    loop {
        let next_announce = announcer.next_default_announce_delay();
        tokio::select! {
            biased;
            command = command_rx.recv() => {
                match command {
                    Some(TrackerActorCommand::Completed(reply)) => {
                        announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                        let downloaded = progress.completed_length();
                        let uploaded = progress.upload_length();
                        announcer
                            .announce_completed(
                                &info_hash,
                                &peer_id,
                                downloaded,
                                uploaded,
                            )
                            .await;
                        let _ = reply.send(());
                    }
                    Some(TrackerActorCommand::Stop(reply)) => {
                        announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                        let downloaded = progress.completed_length();
                        let uploaded = progress.upload_length();
                        announcer
                            .announce_stopped(
                                &info_hash,
                                &peer_id,
                                downloaded,
                                total_size.saturating_sub(downloaded),
                                uploaded,
                            )
                            .await;
                        let _ = reply.send(announcer);
                        return;
                    }
                    None => return,
                }
            }
            _ = wait_for_public_tracker_update(&mut public_tracker_updates) => {
                let added = announcer.sync_public_trackers(MAX_PUBLIC_TRACKERS_TO_TRY).await;
                if added > 0 {
                    debug!(added, "Added refreshed public trackers to active torrent");
                }
            }
            _ = wait_until_announce(next_announce) => {
                announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                if !announcer.is_default_announce_ready() {
                    continue;
                }
                let downloaded = progress.completed_length();
                let uploaded = progress.upload_length();
                if let Some(result) = announcer
                    .announce(
                        &info_hash,
                        &peer_id,
                        downloaded,
                        total_size.saturating_sub(downloaded),
                        uploaded,
                    )
                    .await
                {
                    publish_tracker_peers(&peer_event_tx, result).await;
                }
            }
        }
    }
}

async fn wait_until_announce(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

async fn wait_for_public_tracker_update(updates: &mut Option<tokio::sync::watch::Receiver<u64>>) {
    let closed = match updates.as_mut() {
        Some(updates) => updates.changed().await.is_err(),
        None => std::future::pending().await,
    };
    if closed {
        *updates = None;
    }
}

async fn publish_tracker_peers(event_tx: &Option<mpsc::Sender<PeerEvent>>, result: AnnounceResult) {
    let peers = to_peer_addrs(result);
    if peers.is_empty() {
        return;
    }
    if let Some(event_tx) = event_tx {
        let _ = event_tx.send(PeerEvent::TrackerPeers { peers }).await;
    }
}

fn to_peer_addrs(
    result: AnnounceResult,
) -> Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> {
    result
        .peers
        .into_iter()
        .map(|(ip, port)| aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::wait_for_public_tracker_update;
    use std::time::Duration;

    #[tokio::test]
    async fn closed_catalog_watch_is_removed_instead_of_waking_forever() {
        let (sender, receiver) = tokio::sync::watch::channel(0);
        drop(sender);
        let mut updates = Some(receiver);

        wait_for_public_tracker_update(&mut updates).await;
        assert!(updates.is_none());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                wait_for_public_tracker_update(&mut updates),
            )
            .await
            .is_err()
        );
    }
}
