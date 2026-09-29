use super::BtTrackerAnnouncerActor;
use crate::engine::bittorrent::download::command::BtRuntimeState;
use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;
use crate::request::request_group::AtomicProgress;
use aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::mock_tracker_fixture as mock_tracker;

async fn serve_tracker_list(urls: &[String]) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let body = urls.join("\n");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local tracker catalog source");
    let source = format!("http://{}/trackers.txt", listener.local_addr().unwrap());
    let source_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept catalog request");
        let mut reader = tokio::io::BufReader::new(&mut stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("read request line");
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .await
                .expect("read request header")
                == 0
                || line == "\r\n"
            {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        reader
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .expect("respond with tracker catalog");
    });
    (source, source_task)
}

async fn update_public_tracker_catalog(catalog: &PublicTrackerList, urls: &[String]) {
    let (source, source_task) = serve_tracker_list(urls).await;
    catalog
        .fetch_and_update(&source)
        .await
        .expect("load local tracker catalog");
    source_task.await.expect("catalog source should exit");
}

async fn public_tracker_catalog(urls: &[String]) -> Arc<PublicTrackerList> {
    let catalog = Arc::new(PublicTrackerList::new());
    update_public_tracker_catalog(&catalog, urls).await;
    catalog
}

struct GatedTracker {
    addr: std::net::SocketAddr,
    request_seen: Arc<Notify>,
    release_response: Arc<Notify>,
    task: JoinHandle<()>,
}

impl GatedTracker {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gated tracker");
        let addr = listener.local_addr().expect("gated tracker address");
        let request_seen = Arc::new(Notify::new());
        let release_response = Arc::new(Notify::new());
        let task_request_seen = Arc::clone(&request_seen);
        let task_release_response = Arc::clone(&release_response);
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept announce");
            let mut reader = tokio::io::BufReader::new(&mut stream);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("read request line");
            loop {
                line.clear();
                if reader
                    .read_line(&mut line)
                    .await
                    .expect("read request header")
                    == 0
                    || line == "\r\n"
                {
                    break;
                }
            }
            task_request_seen.notify_one();
            task_release_response.notified().await;
            let body = b"d8:intervali300e5:peers0:e";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .expect("write tracker response headers");
            reader
                .get_mut()
                .write_all(body)
                .await
                .expect("write tracker response body");
        });
        Self {
            addr,
            request_seen,
            release_response,
            task,
        }
    }

    fn announce_url(&self) -> String {
        format!("http://{}/announce", self.addr)
    }
}

impl Drop for GatedTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn actor_fans_out_to_three_public_trackers_and_aggregates_rpc_state() {
    let primary =
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await;
    let dynamic_tracker =
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await;
    let dynamic_tracker_sibling =
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await;
    let public = [
        mock_tracker::MockTrackerServer::start_with_dynamic_announce_list(
            Vec::new(),
            300,
            vec![vec![
                dynamic_tracker.announce_url(),
                dynamic_tracker_sibling.announce_url(),
            ]],
            None,
        )
        .await,
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await,
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await,
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await,
    ];
    let public_urls = public
        .iter()
        .map(mock_tracker::MockTrackerServer::announce_url)
        .collect::<Vec<_>>();
    let catalog = public_tracker_catalog(&public_urls[..1]).await;

    let mut announcer = TrackerAnnouncer::new(&[vec![primary.announce_url()]], &None);
    announcer.set_timeouts(Duration::from_secs(1), Duration::from_secs(1));
    announcer.set_stopped_timeout(Duration::from_secs(1));
    announcer.set_public_tracker_catalog(Arc::clone(&catalog), Default::default());
    let shared = Arc::new(std::sync::RwLock::new(Default::default()));
    announcer.set_runtime_snapshot(Arc::clone(&shared));

    let (actor, peers) = BtTrackerAnnouncerActor::start(
        announcer,
        [0; 20],
        [1; 20],
        100,
        Arc::new(AtomicProgress::new()),
        Arc::new(BtRuntimeState::new(64)),
        true,
        None,
    )
    .await;
    assert!(peers.is_empty());
    primary.wait_for_event("started").await;
    public[0].wait_for_event("started").await;

    update_public_tracker_catalog(&catalog, &public_urls).await;
    for tracker in &public[1..3] {
        tracker.wait_for_event("started").await;
    }
    assert!(public[3].captured_queries().await.is_empty());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let succeeded = shared
                .read()
                .expect("read aggregated tracker state")
                .trackers
                .iter()
                .filter(|tracker| tracker.status == "succeeded")
                .count();
            if succeeded == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("all primary and active public tracker responses should be aggregated");

    let snapshot = shared
        .read()
        .expect("read aggregated tracker state")
        .clone();
    assert_eq!(snapshot.trackers.len(), 7);
    assert!(
        snapshot
            .trackers
            .iter()
            .any(|tracker| tracker.uri == dynamic_tracker.announce_url())
    );
    let dynamic_tier = snapshot
        .trackers
        .iter()
        .find(|tracker| tracker.uri == dynamic_tracker.announce_url())
        .expect("dynamic tracker is present in the aggregate")
        .tier;
    assert_eq!(
        snapshot
            .trackers
            .iter()
            .find(|tracker| tracker.uri == dynamic_tracker_sibling.announce_url())
            .expect("dynamic tier sibling is present in the aggregate")
            .tier,
        dynamic_tier,
        "dynamic announce-list tier grouping should survive aggregation"
    );
    assert_eq!(
        snapshot
            .trackers
            .iter()
            .filter(|tracker| tracker.status == "succeeded")
            .count(),
        4
    );
    assert_eq!(
        snapshot
            .trackers
            .iter()
            .filter(|tracker| tracker.status == "idle")
            .count(),
        1
    );

    actor.announce_completed().await;
    primary.wait_for_event("completed").await;
    for tracker in &public[..3] {
        tracker.wait_for_event("completed").await;
    }

    assert!(actor.stop().await.is_some());
    primary.wait_for_event("stopped").await;
    for tracker in &public[..3] {
        tracker.wait_for_event("stopped").await;
    }
}

#[tokio::test]
async fn slow_public_announce_does_not_hold_up_initial_peer_discovery() {
    let primary =
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await;
    let slow_public = GatedTracker::start().await;
    let catalog = public_tracker_catalog(&[slow_public.announce_url()]).await;
    let mut announcer = TrackerAnnouncer::new(&[vec![primary.announce_url()]], &None);
    announcer.set_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    announcer.set_stopped_timeout(Duration::from_millis(100));
    announcer.set_public_tracker_catalog(catalog, Default::default());

    let (actor, peers) = tokio::time::timeout(
        Duration::from_secs(1),
        BtTrackerAnnouncerActor::start(
            announcer,
            [0; 20],
            [1; 20],
            100,
            Arc::new(AtomicProgress::new()),
            Arc::new(BtRuntimeState::new(64)),
            true,
            None,
        ),
    )
    .await
    .expect("a pending public tracker must not block torrent peer discovery");
    assert!(peers.is_empty());
    tokio::time::timeout(Duration::from_secs(1), slow_public.request_seen.notified())
        .await
        .expect("public tracker announce should be in flight concurrently");

    slow_public.release_response.notify_one();
    tokio::time::timeout(Duration::from_secs(2), actor.stop())
        .await
        .expect("tracker actor should stop after its in-flight announce resolves");
}

#[tokio::test]
async fn stop_command_is_serviced_while_initial_public_announce_is_pending() {
    let primary =
        mock_tracker::MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 300)
            .await;
    let slow_public = GatedTracker::start().await;
    let catalog = public_tracker_catalog(&[slow_public.announce_url()]).await;
    let mut announcer = TrackerAnnouncer::new(&[vec![primary.announce_url()]], &None);
    announcer.set_timeouts(Duration::from_secs(5), Duration::from_secs(1));
    announcer.set_stopped_timeout(Duration::from_millis(100));
    announcer.set_public_tracker_catalog(catalog, Default::default());
    let shared = Arc::new(std::sync::RwLock::new(Default::default()));
    announcer.set_runtime_snapshot(Arc::clone(&shared));

    let (actor, peers) = BtTrackerAnnouncerActor::start(
        announcer,
        [0; 20],
        [1; 20],
        100,
        Arc::new(AtomicProgress::new()),
        Arc::new(BtRuntimeState::new(64)),
        true,
        None,
    )
    .await;
    assert!(peers.is_empty());
    primary.wait_for_event("started").await;
    tokio::time::timeout(Duration::from_secs(1), slow_public.request_seen.notified())
        .await
        .expect("public announce should remain in flight");

    let actor_to_stop = actor.clone();
    let mut stop_task = tokio::spawn(async move { actor_to_stop.stop().await });
    let stopped_while_announce_pending =
        tokio::time::timeout(Duration::from_millis(500), &mut stop_task)
            .await
            .is_ok();

    if !stopped_while_announce_pending {
        slow_public.release_response.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut stop_task).await;
    }

    assert!(
        stopped_while_announce_pending,
        "stop must not wait for a stalled initial public tracker announce"
    );
    primary.wait_for_event("stopped").await;
    let snapshot = shared.read().expect("read tracker runtime snapshot");
    assert!(
        snapshot
            .trackers
            .iter()
            .all(|tracker| tracker.in_flight == 0),
        "cancelling startup announces must clear RPC in-flight state"
    );
}
