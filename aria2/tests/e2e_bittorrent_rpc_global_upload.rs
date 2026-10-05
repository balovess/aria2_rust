#![cfg(feature = "bittorrent")]

//! Process-wide BT upload-limit coverage across RPC-created torrents.

#[path = "support/mod.rs"]
mod support;

#[path = "support/bittorrent_upload.rs"]
mod upload_fixture;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use support::RunningAria2;
use upload_fixture::{
    DEFAULT_BURST_LENGTH, PIECE_LENGTH, PartialSeeder, UPLOAD_RATE_BYTES_PER_SEC,
    connect_interested_leecher, receive_piece, request_piece_blocks, reserve_loopback_port, rpc,
    upload_rate_torrent,
};

fn add_torrent(client: &RunningAria2, id: u64, torrent: Vec<u8>) -> String {
    rpc(
        client,
        id,
        "aria2.addTorrent",
        json!([
            base64::engine::general_purpose::STANDARD.encode(torrent),
            [],
            {}
        ]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned()
}

async fn wait_for_piece(client: &RunningAria2, gid: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = rpc(
                client,
                20,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            if status["completedLength"].as_str() == Some(PIECE_LENGTH.to_string().as_str()) {
                assert_eq!(status["status"], "active");
                assert_eq!(
                    status["totalLength"].as_str(),
                    Some((PIECE_LENGTH * 2).to_string().as_str())
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first verified piece must finish while each tail stays withheld");
}

async fn wait_for_complete(client: &RunningAria2, gid: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                client,
                21,
                "aria2.tellStatus",
                json!([gid, ["completedLength", "totalLength"]]),
            );
            if status["completedLength"] == status["totalLength"] {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("torrent tail must complete after the wire upload assertion");
}

async fn wait_for_uploaded_bytes(client: &RunningAria2, gid: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = rpc(
                client,
                22,
                "aria2.tellStatus",
                json!([gid, ["uploadLength"]]),
            );
            if status["uploadLength"].as_str() == Some(PIECE_LENGTH.to_string().as_str()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("RPC uploadLength must reflect the completed wire transfer");
}

struct TwoTorrentProcess {
    output_dir: tempfile::TempDir,
    client: RunningAria2,
    listen_port: u16,
    meta_a: TorrentMeta,
    meta_b: TorrentMeta,
    payload: Arc<Vec<u8>>,
    gid_a: String,
    gid_b: String,
    peer_a: PartialSeeder,
    peer_b: PartialSeeder,
    _tracker_a: MockTrackerServer,
    _tracker_b: MockTrackerServer,
}

impl TwoTorrentProcess {
    async fn start(initial_upload_limit: &str) -> Self {
        let output_dir = tempfile::tempdir().expect("temporary download directory");
        let placeholder_tracker = MockTrackerServer::start(0).await;
        let placeholder_url = placeholder_tracker.announce_url();
        let placeholder_a = upload_rate_torrent(&placeholder_url, "global-upload-a.bin");
        let placeholder_b = upload_rate_torrent(&placeholder_url, "global-upload-b.bin");
        let meta_a = TorrentMeta::parse(&placeholder_a).expect("first torrent metadata parses");
        let meta_b = TorrentMeta::parse(&placeholder_b).expect("second torrent metadata parses");
        let payload = Arc::new([vec![0x41; PIECE_LENGTH], vec![0x42; PIECE_LENGTH]].concat());
        let peer_a = PartialSeeder::start(meta_a.info_hash.bytes, Arc::clone(&payload)).await;
        let peer_b = PartialSeeder::start(meta_b.info_hash.bytes, Arc::clone(&payload)).await;
        drop(placeholder_tracker);

        let tracker_a = MockTrackerServer::start(peer_a.addr.port()).await;
        let tracker_b = MockTrackerServer::start(peer_b.addr.port()).await;
        let torrent_a = upload_rate_torrent(&tracker_a.announce_url(), "global-upload-a.bin");
        let torrent_b = upload_rate_torrent(&tracker_b.announce_url(), "global-upload-b.bin");
        let listen_port = reserve_loopback_port();
        let args = [
            format!("--dir={}", output_dir.path().display()),
            format!("--listen-port={listen_port}"),
            format!("--max-overall-upload-limit={initial_upload_limit}"),
            "--enable-dht=false".to_owned(),
            "--enable-public-trackers=false".to_owned(),
            "--enable-peer-exchange=false".to_owned(),
            "--bt-enable-web-seed=false".to_owned(),
            "--seed-time=3600".to_owned(),
        ];

        // Start RPC-only so global rate changes and task creation both cross
        // the real application RPC boundary.
        let client = RunningAria2::start_rpc(&args);
        let gid_a = add_torrent(&client, 1, torrent_a);
        let gid_b = add_torrent(&client, 2, torrent_b);
        wait_for_piece(&client, &gid_a).await;
        wait_for_piece(&client, &gid_b).await;

        Self {
            output_dir,
            client,
            listen_port,
            meta_a,
            meta_b,
            payload,
            gid_a,
            gid_b,
            peer_a,
            peer_b,
            _tracker_a: tracker_a,
            _tracker_b: tracker_b,
        }
    }

    async fn connect_leechers(&self) -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let leecher_a =
            connect_interested_leecher(self.listen_port, self.meta_a.info_hash.bytes).await;
        let leecher_b =
            connect_interested_leecher(self.listen_port, self.meta_b.info_hash.bytes).await;
        (leecher_a, leecher_b)
    }

    async fn assert_upload_counters_and_finish(&self) {
        wait_for_uploaded_bytes(&self.client, &self.gid_a).await;
        wait_for_uploaded_bytes(&self.client, &self.gid_b).await;
        self.peer_a.release_tail();
        self.peer_b.release_tail();
        wait_for_complete(&self.client, &self.gid_a).await;
        wait_for_complete(&self.client, &self.gid_b).await;
        assert_eq!(
            std::fs::read(self.output_dir.path().join("global-upload-a.bin")).unwrap(),
            *self.payload
        );
        assert_eq!(
            std::fs::read(self.output_dir.path().join("global-upload-b.bin")).unwrap(),
            *self.payload
        );
    }
}

async fn upload_both_pieces(
    leecher_a: &mut tokio::net::TcpStream,
    leecher_b: &mut tokio::net::TcpStream,
) -> (Vec<u8>, Vec<u8>, Duration) {
    let started = Instant::now();
    request_piece_blocks(leecher_a, 0).await;
    request_piece_blocks(leecher_b, 0).await;
    let (uploaded_a, uploaded_b) = tokio::time::timeout(Duration::from_secs(25), async {
        tokio::join!(receive_piece(leecher_a, 0), receive_piece(leecher_b, 0))
    })
    .await
    .expect("both globally rate-limited uploads must finish within 25 seconds");
    (uploaded_a, uploaded_b, started.elapsed())
}

fn assert_global_rate_window(total_bytes: usize, elapsed: Duration) {
    let minimum_wait = Duration::from_millis(
        (((total_bytes - DEFAULT_BURST_LENGTH) * 3 * 1000) / (UPLOAD_RATE_BYTES_PER_SEC * 4))
            as u64,
    );
    assert!(
        elapsed >= minimum_wait,
        "combined torrents should share the {} KiB/s process limit after the {} KiB burst; took {elapsed:?}, expected at least {minimum_wait:?}",
        UPLOAD_RATE_BYTES_PER_SEC / 1024,
        DEFAULT_BURST_LENGTH / 1024,
    );
}

#[tokio::test]
async fn cli_global_upload_limit_aggregates_rpc_added_torrents_on_peer_wire() {
    let setup = TwoTorrentProcess::start(&format!("{}K", UPLOAD_RATE_BYTES_PER_SEC / 1024)).await;
    let (mut leecher_a, mut leecher_b) = setup.connect_leechers().await;
    let (uploaded_a, uploaded_b, elapsed) =
        upload_both_pieces(&mut leecher_a, &mut leecher_b).await;

    assert_eq!(uploaded_a, setup.payload[..PIECE_LENGTH]);
    assert_eq!(uploaded_b, setup.payload[..PIECE_LENGTH]);
    assert_global_rate_window(PIECE_LENGTH * 2, elapsed);
    setup.assert_upload_counters_and_finish().await;
}

#[tokio::test]
async fn rpc_change_global_upload_limit_updates_live_torrent_peer_actors() {
    let setup = TwoTorrentProcess::start("16K").await;
    let (mut leecher_a, mut leecher_b) = setup.connect_leechers().await;

    assert_eq!(
        rpc(
            &setup.client,
            30,
            "aria2.changeGlobalOption",
            json!([{"max-overall-upload-limit": format!("{}K", UPLOAD_RATE_BYTES_PER_SEC / 1024)}]),
        ),
        "OK"
    );

    let (uploaded_a, uploaded_b, elapsed) =
        upload_both_pieces(&mut leecher_a, &mut leecher_b).await;
    assert_eq!(uploaded_a, setup.payload[..PIECE_LENGTH]);
    assert_eq!(uploaded_b, setup.payload[..PIECE_LENGTH]);
    assert_global_rate_window(PIECE_LENGTH * 2, elapsed);
    setup.assert_upload_counters_and_finish().await;
}
