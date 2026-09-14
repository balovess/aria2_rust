//! End-to-end RPC snapshots for live BitTorrent runtime state.
//!
//! The fixture uses a real local tracker announce, a real TCP seeder/leecher
//! transfer, and a real local DHT engine before reading the RPC snapshots.

#![cfg(feature = "bittorrent")]

#[path = "../support/mod.rs"]
mod support;

#[path = "../../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_core::engine::bt_registry::{BtObject, BtRegistry};
use aria2_core::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use aria2_core::engine::bt_tracker_comm::{BtAnnounce, TrackerAnnouncer, TrackerRuntimeSnapshot};
use aria2_core::engine::bt_upload_session::{BtSeedingConfig, InMemoryPieceProvider};
use aria2_core::request::request_group::{BtPeerSnapshot, BtPeerSource};
use aria2_protocol::bittorrent::dht::engine::{DhtEngine, DhtEngineConfig, DhtEngineState};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_rpc::json_rpc::{JsonRpcRequest, JsonRpcResponse};
use std::sync::Arc;
use support::rpc::RpcFixture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest::new(method, params).with_id(1)
}

fn assert_success(resp: &JsonRpcResponse) {
    assert!(
        resp.is_success(),
        "Expected success response, got error: {:?}",
        resp.error
    );
}

/// Test: real BitTorrent peer, tracker, and DHT runtime state reaches RPC snapshots.
#[tokio::test]
async fn rpc_snapshots_expose_real_bt_peer_tracker_and_dht_state() {
    let info_hash = [0x52u8; 20];
    let seeder_peer_id = [0x62u8; 20];
    let leecher_peer_id = [0x72u8; 20];
    let piece = (0..16 * 1024)
        .map(|index| (index as u8).wrapping_mul(13))
        .collect::<Vec<_>>();
    let piece_len = piece.len();

    let tracker = mock_tracker::MockTrackerServer::start_with_peers(vec![65535], false).await;
    let tracker_url = tracker.announce_url();
    let tracker_runtime = Arc::new(std::sync::RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut tracker_announcer = TrackerAnnouncer::new(&[], &Some(tracker_url.clone()));
    tracker_announcer.set_runtime_snapshot(Arc::clone(&tracker_runtime));
    let announce = tracker_announcer
        .announce(&info_hash, &seeder_peer_id, piece_len as u64, 0, 0)
        .await
        .expect("the local tracker should return a live announce result");
    assert_eq!(announce.peers, vec![("127.0.0.1".to_string(), 65535)]);
    tracker.wait_for_event("started").await;

    let dht = DhtEngine::start(DhtEngineConfig::local())
        .await
        .expect("the local DHT engine should start");
    assert_eq!(dht.stats().await.state, DhtEngineState::Running);

    let registry = Arc::new(std::sync::RwLock::new(BtRegistry::new()));
    let fixture = RpcFixture::new_with_bt_registry(None, Arc::clone(&registry));
    let engine = &fixture.engine;

    let add_resp = engine
        .handle_request(&make_request(
            "aria2.addUri",
            serde_json::json!([["bt://local-seeder/rpc-snapshot-piece"]]),
        ))
        .await;
    assert_success(&add_resp);
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();
    let gid_value = u64::from_str_radix(&gid, 16).expect("RPC GID should be hexadecimal");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let leecher_piece = piece.clone();
    let leecher = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(&Handshake::new(&info_hash, &leecher_peer_id).to_bytes())
            .await
            .unwrap();
        let mut response = [0u8; 68];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(Handshake::parse(&response).unwrap().info_hash, info_hash);

        let mut length = [0u8; 4];
        stream.read_exact(&mut length).await.unwrap();
        let mut availability = vec![0u8; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut availability).await.unwrap();
        assert_eq!(availability, vec![5, 0x80]);

        stream.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // Interested
        loop {
            stream.read_exact(&mut length).await.unwrap();
            let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut payload).await.unwrap();
            if payload.first() == Some(&1) {
                break;
            }
        }

        let mut request = Vec::with_capacity(17);
        request.extend_from_slice(&13u32.to_be_bytes());
        request.push(6); // Request
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&(piece_len as u32).to_be_bytes());
        stream.write_all(&request).await.unwrap();

        stream.read_exact(&mut length).await.unwrap();
        let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut payload).await.unwrap();
        assert_eq!(payload.first(), Some(&7));
        assert_eq!(u32::from_be_bytes(payload[1..5].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(payload[5..9].try_into().unwrap()), 0);
        assert_eq!(&payload[9..], leecher_piece.as_slice());
    });
    let (server_stream, leecher_addr) = listener.accept().await.unwrap();
    let peer_connection =
        aria2_protocol::bittorrent::peer::connection::PeerConnection::from_incoming_stream(
            server_stream,
            &info_hash,
            &seeder_peer_id,
        )
        .await
        .expect("the seeder should complete the real leecher handshake");
    assert_eq!(peer_connection.remote_peer_id, Some(leecher_peer_id));
    assert_eq!(peer_connection.remote_addr(), Some(leecher_addr));
    let peer_id = peer_connection
        .remote_peer_id
        .expect("handshake should provide the leecher peer-id");
    let peer_addr = peer_connection
        .remote_addr()
        .expect("handshake should provide the leecher endpoint");

    let mut provider = InMemoryPieceProvider::new(piece_len as u32, 1);
    provider.set_piece_data(0, piece);
    let mut seed_manager = BtSeedManager::new_with_info_hash(
        info_hash,
        vec![peer_connection],
        Arc::new(provider),
        BtSeedingConfig::default(),
        SeedExitCondition::with_ratio(1.0),
        piece_len as u64,
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        seed_manager.run_seeding_loop(),
    )
    .await
    .expect("the real seeder should finish after the leecher receives one piece")
    .expect("the real seeder should not fail");
    leecher.await.unwrap();
    let (upload_length, upload_speed) = seed_manager.get_upload_stats();
    assert!(
        upload_length > 0,
        "real seeder uploadLength should increase"
    );
    assert!(upload_speed > 0, "real seeder uploadSpeed should increase");

    let peer_snapshot = BtPeerSnapshot {
        peer_id,
        addr: peer_addr,
        is_incoming: true,
        source: BtPeerSource::Incoming,
        bitfield: Some(vec![0x80]),
        uploaded_bytes: upload_length,
        downloaded_bytes: 0,
        upload_speed: upload_speed as f64,
        download_speed: 0.0,
        avg_upload_speed: upload_speed,
        avg_download_speed: 0,
        am_choking: false,
        peer_choking: false,
        seeder: Some(false),
        connection_duration_secs: 1,
        last_data_age_secs: 0,
        is_snubbed: false,
        is_banned: false,
    };

    let group = fixture
        .group_man
        .group_by_hex(&gid)
        .expect("RPC-created group must be discoverable");
    {
        let group = group.write().unwrap();
        group.set_bt_metadata(1, 16_384, "52".repeat(20));
        group.set_bt_bitfield(Some(vec![0x80]));
        group.set_uploaded_length(upload_length);
        group.set_upload_speed_cached(upload_speed);
        group.set_bt_peer_snapshots(vec![peer_snapshot]);
    }
    assert_eq!(fixture.group_man.fill_from_reserver().len(), 1);

    let tracker_announce = Arc::new(BtAnnounce::new(&[], &Some(tracker_url.clone())));
    let bt_object = BtObject::builder()
        .bt_announce(tracker_announce)
        .tracker_runtime(Arc::clone(&tracker_runtime))
        .build();
    {
        let mut registry = registry.write().unwrap();
        registry.put(gid_value, bt_object);
        registry.set_dht_engine_for_gid(gid_value, Arc::clone(&dht));
    }

    let peers_resp = engine
        .handle_request(&make_request("aria2.getPeers", serde_json::json!([gid])))
        .await;
    assert_success(&peers_resp);
    let peers = peers_resp.result.unwrap();
    assert_eq!(peers.as_array().unwrap().len(), 1);
    assert_eq!(peers[0]["peerId"], "72".repeat(20));
    assert_eq!(peers[0]["ip"], "127.0.0.1");
    assert_eq!(peers[0]["port"], "0");
    assert_eq!(peers[0]["bitfield"], "80");
    assert_eq!(peers[0]["amChoking"], "false");
    assert_eq!(peers[0]["peerChoking"], "false");
    assert_eq!(peers[0]["uploadSpeed"], upload_speed.to_string());
    assert_eq!(peers[0]["seeder"], "false");

    let trackers_resp = engine
        .handle_request(&make_request("aria2.getTrackers", serde_json::json!([gid])))
        .await;
    assert_success(&trackers_resp);
    let trackers = trackers_resp.result.unwrap();
    assert_eq!(trackers.as_array().unwrap().len(), 1);
    assert_eq!(trackers[0]["uri"], tracker_url);
    assert_eq!(trackers[0]["tier"], 1);
    assert_eq!(trackers[0]["current"], true);
    assert_eq!(trackers[0]["lastAttempt"], true);
    assert_eq!(trackers[0]["interval"], "300");
    assert_eq!(trackers[0]["seeders"], 1);
    assert_eq!(trackers[0]["leechers"], 1);

    let dht_resp = engine
        .handle_request(&make_request("aria2.getDhtStatus", serde_json::json!([])))
        .await;
    assert_success(&dht_resp);
    let dht_status = dht_resp.result.unwrap();
    assert_eq!(dht_status["state"], "running");
    assert_eq!(dht_status["totalNodes"], "0");
    assert_eq!(dht_status["goodNodes"], "0");
    assert_eq!(dht_status["pendingTransactions"], "0");

    dht.shutdown_async().await;
}
