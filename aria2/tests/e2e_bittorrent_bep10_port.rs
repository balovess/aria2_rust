#![cfg(feature = "bittorrent")]

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::time::Duration;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::message::serializer::serialize;
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use support::RunningAria2;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve an ephemeral BT listener port")
        .local_addr()
        .expect("bound listener has an address")
        .port()
}

fn torrent(tracker_url: &str) -> (Vec<u8>, [u8; 20]) {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(1));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"bep10-port.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(1));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let bytes = BencodeValue::Dict(root).encode();
    let metadata = TorrentMeta::parse(&bytes).expect("test torrent should parse");
    (bytes, metadata.info_hash.bytes)
}

fn rpc(client: &RunningAria2, id: u64, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
    let response = client.post(
        "/jsonrpc",
        "application/json",
        request.to_string().as_bytes(),
    );
    assert_eq!(
        response.status, 200,
        "RPC HTTP response: {:?}",
        response.headers
    );
    let response: Value = serde_json::from_slice(&response.body).expect("RPC JSON response");
    assert!(response.get("error").is_none(), "RPC error: {response}");
    response["result"].clone()
}

#[tokio::test]
async fn incoming_peer_bep10_port_is_exposed_as_advertised_rpc_endpoint() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let tracker = MockTrackerServer::start(0).await;
    let (torrent, info_hash) = torrent(&tracker.announce_url());
    let listen_port = reserve_loopback_port();
    let client = RunningAria2::start_rpc(&[
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
    ]);
    let encoded = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(&client, 1, "aria2.addTorrent", json!([encoded, [], {}]))
        .as_str()
        .expect("addTorrent returns a GID")
        .to_owned();

    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "totalLength"]]),
        );
        if status["status"] == "active" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "torrent did not become active: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut peer = TcpStream::connect(("127.0.0.1", listen_port))
        .await
        .expect("connect an incoming test peer");
    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[25] |= 0x10;
    handshake[28..48].copy_from_slice(&info_hash);
    handshake[48..68].fill(0x51);
    peer.write_all(&handshake)
        .await
        .expect("send incoming BitTorrent handshake");
    let mut response = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(3), peer.read_exact(&mut response))
        .await
        .expect("incoming handshake response timed out")
        .expect("read incoming handshake response");
    assert_eq!(&response[28..48], &info_hash);

    let mut extensions = BTreeMap::new();
    extensions.insert(b"ut_pex".to_vec(), BencodeValue::Int(19));
    let payload = BencodeValue::Dict(BTreeMap::from([
        (b"m".to_vec(), BencodeValue::Dict(extensions)),
        (b"p".to_vec(), BencodeValue::Int(6881)),
    ]))
    .encode();
    let frame = serialize(&BtMessage::Extended { ext_id: 0, payload });
    peer.write_all(&frame)
        .await
        .expect("send BEP 10 extension handshake with the advertised port");

    let details_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let details = loop {
        let details = rpc(&client, 3, "aria2.getPeerDetails", json!([gid]));
        let peer_details = details
            .as_array()
            .and_then(|peers| peers.iter().find(|peer| peer["source"] == "incoming"));
        if peer_details.is_some() {
            break peer_details.cloned().expect("checked incoming peer");
        }
        assert!(
            tokio::time::Instant::now() < details_deadline,
            "incoming peer did not appear in RPC: {details}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert_eq!(
        details["port"], 6881,
        "RPC should report the BEP 10 listen port, not the TCP source port"
    );
    assert_eq!(
        details["flags"]["incoming"], false,
        "aria2 clears the incoming-peer flag once the peer advertises its listen port"
    );
    let peers = rpc(&client, 4, "aria2.getPeers", json!([gid]));
    assert!(
        peers.as_array().is_some_and(|peers| peers
            .iter()
            .any(|peer| { peer["ip"] == "127.0.0.1" && peer["port"] == "6881" })),
        "aria2.getPeers should serialize the advertised BEP 10 port: {peers}"
    );

    drop(peer);
    let disconnect_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let details = rpc(&client, 5, "aria2.getPeerDetails", json!([gid]));
        let peers = rpc(&client, 6, "aria2.getPeers", json!([gid]));
        if details.as_array().is_some_and(Vec::is_empty)
            && peers.as_array().is_some_and(Vec::is_empty)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < disconnect_deadline,
            "closed transport peer remained in RPC snapshots: details={details}, peers={peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
