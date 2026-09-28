//! Torrent-scoped owner for tracker announce state and its protocol deadlines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::engine::bittorrent::download::command::{BtRuntimeState, MAX_PUBLIC_TRACKERS_TO_TRY};
use crate::engine::bittorrent::peer::message_handler::PeerEvent;
use crate::engine::bittorrent::tracker::communication::{AnnounceResult, TrackerAnnouncer};
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

    let mut pending_tracker_peers = None;
    loop {
        // Keep the tracker peer batch pending under bounded-channel
        // backpressure while continuing to service actor commands. Pausing
        // periodic announces here prevents an unbounded peer backlog.
        let next_announce = if pending_tracker_peers.is_some() {
            None
        } else {
            announcer.next_default_announce_delay()
        };
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
            permit = reserve_tracker_peer_event_slot(
                &peer_event_tx,
                pending_tracker_peers.is_some(),
            ) => {
                if let Some(permit) = permit {
                    let peers = pending_tracker_peers
                        .take()
                        .expect("a peer-event permit is requested only for a pending batch");
                    permit.send(PeerEvent::TrackerPeers { peers });
                } else {
                    // A closed receiver can never accept the pending batch.
                    // Clear it so the branch cannot become a busy loop.
                    pending_tracker_peers = None;
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
                    let peers = to_peer_addrs(result);
                    if !peers.is_empty() && peer_event_tx.is_some() {
                        pending_tracker_peers = Some(peers);
                    }
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

async fn reserve_tracker_peer_event_slot<'a>(
    event_tx: &'a Option<mpsc::Sender<PeerEvent>>,
    has_pending_peers: bool,
) -> Option<mpsc::Permit<'a, PeerEvent>> {
    match event_tx {
        Some(event_tx) if has_pending_peers => event_tx.reserve().await.ok(),
        _ => std::future::pending().await,
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
#[path = "../../../../../tests/fixtures/mock_tracker.rs"]
mod mock_tracker_fixture;

#[cfg(test)]
mod tests {
    use super::{
        BtTrackerAnnouncerActor, reserve_tracker_peer_event_slot, wait_for_public_tracker_update,
    };
    use crate::engine::bittorrent::download::command::BtRuntimeState;
    use crate::engine::bittorrent::peer::message_handler::PeerEvent;
    use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;
    use crate::request::request_group::AtomicProgress;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    use super::mock_tracker_fixture as mock_tracker;

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

    #[tokio::test]
    async fn peer_event_reservation_waits_for_capacity_without_losing_the_batch() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .try_send(PeerEvent::TrackerPeers { peers: Vec::new() })
            .expect("fill the bounded peer-event queue");
        let event_sender = Some(event_tx);
        let mut reservation = Box::pin(reserve_tracker_peer_event_slot(&event_sender, true));
        tokio::select! {
            biased;
            permit = &mut reservation => panic!("full queue unexpectedly returned a permit: {permit:?}"),
            _ = tokio::task::yield_now() => {}
        }

        assert!(matches!(
            event_rx.recv().await,
            Some(PeerEvent::TrackerPeers { .. })
        ));
        let permit = reservation
            .await
            .expect("an open receiver should free a peer-event slot");
        permit.send(PeerEvent::TrackerPeers { peers: Vec::new() });
    }

    #[tokio::test]
    async fn actor_services_completed_and_stop_while_peer_event_queue_is_full() {
        let tracker =
            mock_tracker::MockTrackerServer::start_with_peers_and_interval(vec![6881], false, 1)
                .await;
        let mut announcer = TrackerAnnouncer::new(&[vec![tracker.announce_url()]], &None);
        announcer.set_timeouts(Duration::from_secs(1), Duration::from_secs(1));
        announcer.set_stopped_timeout(Duration::from_secs(1));
        let (peer_event_tx, mut peer_event_rx) = mpsc::channel(1);
        let (actor, initial_peers) = BtTrackerAnnouncerActor::start(
            announcer,
            [0; 20],
            [1; 20],
            1,
            Arc::new(AtomicProgress::new()),
            Arc::new(BtRuntimeState::new(64)),
            false,
            Some(peer_event_tx.clone()),
        )
        .await;
        assert_eq!(initial_peers.len(), 1);

        peer_event_tx
            .try_send(PeerEvent::TrackerPeers { peers: Vec::new() })
            .expect("fill the bounded peer-event queue");
        assert!(
            tracker
                .wait_for_query_count(2, Duration::from_secs(3))
                .await,
            "the periodic announce should complete while the event queue is full"
        );

        tokio::time::timeout(Duration::from_secs(2), actor.announce_completed())
            .await
            .expect("completed announce must remain serviceable under peer-event backpressure");
        tracker.wait_for_event("completed").await;
        assert!(matches!(
            peer_event_rx.recv().await,
            Some(PeerEvent::TrackerPeers { peers }) if peers.is_empty()
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), peer_event_rx.recv())
                .await
                .expect("pending tracker peers should be delivered after capacity returns"),
            Some(PeerEvent::TrackerPeers { peers }) if peers.len() == 1
        ));

        let returned_announcer = tokio::time::timeout(Duration::from_secs(2), actor.stop())
            .await
            .expect("actor stop must not wait forever on a full peer-event queue");
        assert!(returned_announcer.is_some());
        tracker.wait_for_event("stopped").await;
    }
}
