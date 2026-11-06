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
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(vec![6881], false, 1).await;
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
