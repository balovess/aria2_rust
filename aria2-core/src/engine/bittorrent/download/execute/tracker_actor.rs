//! Torrent-scoped owner for tracker announce state and its protocol deadlines.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::engine::bittorrent::download::command::{BtRuntimeState, MAX_PUBLIC_TRACKERS_TO_TRY};
use crate::engine::bittorrent::peer::message_handler::PeerEvent;
use crate::engine::bittorrent::tracker::communication::{SharedTrackerRuntime, TrackerAnnouncer};
use crate::request::request_group::AtomicProgress;

#[path = "tracker_actor_runtime.rs"]
mod runtime;
use runtime::{
    MAX_PUBLIC_TRACKERS_IN_FANOUT, add_public_tracker_announcers, announce_completed_many,
    announce_due_public, announce_if_ready, announce_initial_primary, announce_many,
    announce_stopped_many, merge_pending_peers, next_tracker_announce_delay,
    publish_tracker_runtime, results_to_peer_addrs,
};

enum TrackerActorCommand {
    Completed(oneshot::Sender<()>),
    Stop(oneshot::Sender<()>),
}

struct TrackerActorInner {
    command_tx: mpsc::Sender<TrackerActorCommand>,
    stop_command_lock: Mutex<()>,
    task: Mutex<Option<JoinHandle<()>>>,
    stopped: AtomicBool,
    runtime: Arc<BtRuntimeState>,
}

impl Drop for TrackerActorInner {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
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
        let tracker_runtime = announcer.shared_runtime_snapshot();
        let task = tokio::spawn(async move {
            run_tracker_actor(
                announcer,
                tracker_runtime,
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
                stop_command_lock: Mutex::new(()),
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

    /// Send the terminal stopped event and join the actor task.
    pub(crate) async fn stop(&self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        let stop_command_sent = {
            let _stop_command_guard = self.inner.stop_command_lock.lock().await;
            if self.inner.stopped.load(Ordering::Acquire) {
                false
            } else {
                let sent = self
                    .inner
                    .command_tx
                    .send(TrackerActorCommand::Stop(reply_tx))
                    .await
                    .is_ok();
                if sent {
                    self.inner.stopped.store(true, Ordering::Release);
                }
                sent
            }
        };
        if stop_command_sent {
            let _ = reply_rx.await;
        }
        let mut task = self.inner.task.lock().await;
        if let Some(task_handle) = task.as_mut() {
            let _ = task_handle.await;
        }
        task.take();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_tracker_actor(
    mut announcer: TrackerAnnouncer,
    tracker_runtime: Option<SharedTrackerRuntime>,
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
    let mut public_announcers = Vec::new();
    let mut active_public_urls = HashSet::new();
    let mut public_urls = if enable_public_trackers {
        announcer
            .public_tracker_urls(MAX_PUBLIC_TRACKERS_TO_TRY)
            .await
    } else {
        Vec::new()
    };
    let added = add_public_tracker_announcers(
        &announcer,
        &mut public_announcers,
        &mut active_public_urls,
        MAX_PUBLIC_TRACKERS_IN_FANOUT,
    )
    .await;
    if added > 0 {
        debug!(
            added,
            "Selected public trackers for bounded announce fan-out"
        );
    }

    announcer.set_less_than_min_peers(runtime.less_than_min_peers());
    for public_announcer in &mut public_announcers {
        public_announcer.set_less_than_min_peers(runtime.less_than_min_peers());
    }
    publish_tracker_runtime(
        tracker_runtime.as_ref(),
        &announcer,
        &public_announcers,
        &public_urls,
    );

    // Poll independent announces concurrently and return the first source's
    // peers immediately; a slow public tracker must not hold up peer dialing.
    let left = total_size.saturating_sub(progress.completed_length());
    let mut primary_announce = Box::pin(announce_initial_primary(
        &mut announcer,
        &info_hash,
        &peer_id,
        left,
    ));
    let mut public_pending = public_announcers.len();
    let mut public_announces = FuturesUnordered::new();
    for public_announcer in &mut public_announcers {
        public_announces.push(async move {
            public_announcer
                .announce(&info_hash, &peer_id, 0, left, 0)
                .await
        });
    }
    let mut primary_pending = true;
    let mut started_tx = Some(started_tx);
    let mut pending_tracker_peers = None;
    let mut startup_commands = VecDeque::new();
    let mut stop_received_during_startup = false;
    let mut startup_command_channel_closed = false;
    while primary_pending || public_pending > 0 {
        tokio::select! {
            command = command_rx.recv() => {
                match command {
                    Some(command @ TrackerActorCommand::Completed(_)) => {
                        startup_commands.push_back(command);
                    }
                    Some(command @ TrackerActorCommand::Stop(_)) => {
                        startup_commands.push_back(command);
                        stop_received_during_startup = true;
                        break;
                    }
                    None => {
                        startup_command_channel_closed = true;
                        break;
                    }
                }
            }
            results = &mut primary_announce, if primary_pending => {
                primary_pending = false;
                let peers = results_to_peer_addrs(results);
                if let Some(started_tx) = started_tx.take() {
                    let _ = started_tx.send(peers);
                } else if peer_event_tx.is_some() {
                    pending_tracker_peers = merge_pending_peers(pending_tracker_peers, peers);
                }
            }
            result = public_announces.next(), if public_pending > 0 => {
                public_pending -= 1;
                let peers = results_to_peer_addrs(result.into_iter().flatten());
                if let Some(started_tx) = started_tx.take() {
                    let _ = started_tx.send(peers);
                } else if peer_event_tx.is_some() {
                    pending_tracker_peers = merge_pending_peers(pending_tracker_peers, peers);
                }
            }
        }
    }
    if let Some(started_tx) = started_tx.take() {
        let _ = started_tx.send(Vec::new());
    }
    drop(primary_announce);
    drop(public_announces);
    publish_tracker_runtime(
        tracker_runtime.as_ref(),
        &announcer,
        &public_announcers,
        &public_urls,
    );
    let mut download_complete = false;
    if startup_command_channel_closed {
        return;
    }
    if stop_received_during_startup {
        announcer.cancel_pending_announce();
        for public_announcer in &mut public_announcers {
            public_announcer.cancel_pending_announce();
        }
    }
    loop {
        // Keep the tracker peer batch pending under bounded-channel
        // backpressure while continuing to service actor commands. Pausing
        // periodic announces here prevents an unbounded peer backlog.
        let next_announce = if pending_tracker_peers.is_some() {
            None
        } else {
            next_tracker_announce_delay(&announcer, &public_announcers)
        };
        tokio::select! {
            biased;
            command = async {
                if let Some(command) = startup_commands.pop_front() {
                    Some(command)
                } else {
                    command_rx.recv().await
                }
            } => {
                match command {
                    Some(TrackerActorCommand::Completed(reply)) => {
                        download_complete = true;
                        announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                        for public_announcer in &mut public_announcers {
                            public_announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                        }
                        let downloaded = progress.completed_length();
                        let uploaded = progress.upload_length();
                        let ((), ()) = tokio::join!(
                            announcer.announce_completed(
                                &info_hash,
                                &peer_id,
                                downloaded,
                                uploaded,
                            ),
                            announce_completed_many(
                                &mut public_announcers,
                                &info_hash,
                                &peer_id,
                                downloaded,
                                uploaded,
                            ),
                        );
                        publish_tracker_runtime(
                            tracker_runtime.as_ref(),
                            &announcer,
                            &public_announcers,
                            &public_urls,
                        );
                        let _ = reply.send(());
                    }
                    Some(TrackerActorCommand::Stop(reply)) => {
                        announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                        for public_announcer in &mut public_announcers {
                            public_announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                        }
                        let downloaded = progress.completed_length();
                        let uploaded = progress.upload_length();
                        let ((), ()) = tokio::join!(
                            announcer.announce_stopped(
                                &info_hash,
                                &peer_id,
                                downloaded,
                                total_size.saturating_sub(downloaded),
                                uploaded,
                            ),
                            announce_stopped_many(
                                &mut public_announcers,
                                &info_hash,
                                &peer_id,
                                downloaded,
                                total_size.saturating_sub(downloaded),
                                uploaded,
                            ),
                        );
                        publish_tracker_runtime(
                            tracker_runtime.as_ref(),
                            &announcer,
                            &public_announcers,
                            &public_urls,
                        );
                        let _ = reply.send(());
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
                public_urls = announcer
                    .public_tracker_urls(MAX_PUBLIC_TRACKERS_TO_TRY)
                    .await;
                let previous_count = public_announcers.len();
                let added = add_public_tracker_announcers(
                    &announcer,
                    &mut public_announcers,
                    &mut active_public_urls,
                    MAX_PUBLIC_TRACKERS_IN_FANOUT,
                )
                .await;
                if added > 0 {
                    debug!(added, "Added refreshed public trackers to active torrent");
                    for public_announcer in &mut public_announcers[previous_count..] {
                        public_announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                    }
                    let downloaded = progress.completed_length();
                    let left = total_size.saturating_sub(downloaded);
                    let uploaded = progress.upload_length();
                    let new_results = announce_many(
                        &mut public_announcers[previous_count..],
                        &info_hash,
                        &peer_id,
                        downloaded,
                        left,
                        uploaded,
                    )
                    .await;
                    if download_complete {
                        announce_completed_many(
                            &mut public_announcers[previous_count..],
                            &info_hash,
                            &peer_id,
                            downloaded,
                            uploaded,
                        )
                        .await;
                    }
                    if peer_event_tx.is_some() {
                        pending_tracker_peers = merge_pending_peers(
                            pending_tracker_peers,
                            results_to_peer_addrs(new_results),
                        );
                    }
                }
                publish_tracker_runtime(
                    tracker_runtime.as_ref(),
                    &announcer,
                    &public_announcers,
                    &public_urls,
                );
            }
            _ = wait_until_announce(next_announce) => {
                announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                for public_announcer in &mut public_announcers {
                    public_announcer.set_less_than_min_peers(runtime.less_than_min_peers());
                }
                let downloaded = progress.completed_length();
                let uploaded = progress.upload_length();
                let (primary_results, public_results) = tokio::join!(
                    announce_if_ready(
                        &mut announcer,
                        &info_hash,
                        &peer_id,
                        downloaded,
                        total_size.saturating_sub(downloaded),
                        uploaded,
                    ),
                    announce_due_public(
                        &mut public_announcers,
                        &info_hash,
                        &peer_id,
                        downloaded,
                        total_size.saturating_sub(downloaded),
                        uploaded,
                    ),
                );
                let peers = results_to_peer_addrs(primary_results.into_iter().chain(public_results));
                if !peers.is_empty() && peer_event_tx.is_some() {
                    pending_tracker_peers = merge_pending_peers(pending_tracker_peers, peers);
                }
                publish_tracker_runtime(
                    tracker_runtime.as_ref(),
                    &announcer,
                    &public_announcers,
                    &public_urls,
                );
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

#[cfg(test)]
#[path = "../../../../../tests/fixtures/mock_tracker.rs"]
mod mock_tracker_fixture;

#[cfg(test)]
#[path = "tracker_actor_fanout_tests.rs"]
mod fanout_tests;

#[cfg(test)]
mod tests {
    use super::{
        BtTrackerAnnouncerActor, TrackerActorCommand, TrackerActorInner,
        reserve_tracker_peer_event_slot, wait_for_public_tracker_update,
    };
    use crate::engine::bittorrent::download::command::BtRuntimeState;
    use crate::engine::bittorrent::peer::message_handler::PeerEvent;
    use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;
    use crate::request::request_group::AtomicProgress;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::sync::{Notify, mpsc};

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
            Arc::new(BtRuntimeState::new(Arc::new(AtomicUsize::new(64)))),
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

        tokio::time::timeout(Duration::from_secs(2), actor.stop())
            .await
            .expect("actor stop must not wait forever on a full peer-event queue");
        tracker.wait_for_event("stopped").await;
    }

    #[tokio::test]
    async fn cancelled_tracker_stop_can_be_retried_to_join_actor_task() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let stop_received = Arc::new(Notify::new());
        let release_stop = Arc::new(Notify::new());
        let task_stop_received = Arc::clone(&stop_received);
        let task_release_stop = Arc::clone(&release_stop);
        let task = tokio::spawn(async move {
            if let Some(TrackerActorCommand::Stop(reply)) = command_rx.recv().await {
                task_stop_received.notify_one();
                task_release_stop.notified().await;
                let _ = reply.send(());
            }
        });
        let actor = BtTrackerAnnouncerActor {
            inner: Arc::new(TrackerActorInner {
                command_tx,
                stop_command_lock: super::Mutex::new(()),
                task: super::Mutex::new(Some(task)),
                stopped: std::sync::atomic::AtomicBool::new(false),
                runtime: Arc::new(BtRuntimeState::new(Arc::new(AtomicUsize::new(64)))),
            }),
        };

        {
            let first_stop = actor.stop();
            tokio::pin!(first_stop);
            tokio::time::timeout(Duration::from_secs(1), async {
                tokio::select! {
                    _ = &mut first_stop => panic!("first stop must remain pending until the actor exits"),
                    _ = stop_received.notified() => {}
                }
            })
            .await
            .expect("the actor should receive its single Stop command");
        }

        let retry_stop = actor.stop();
        tokio::pin!(retry_stop);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut retry_stop)
                .await
                .is_err(),
            "a repeated stop must join the still-running actor instead of returning immediately"
        );

        release_stop.notify_one();
        tokio::time::timeout(Duration::from_secs(1), &mut retry_stop)
            .await
            .expect("retry stop should finish after the actor exits");
        let task = actor.inner.task.lock().await;
        assert!(task.is_none(), "a joined tracker task should be removed");
    }
}
