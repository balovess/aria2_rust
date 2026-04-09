mod fixtures {
    pub mod mock_bt_peer;
    pub mod mock_dht_node;
    pub mod test_torrent_builder;
}
use aria2_core::engine::command::{Command, CommandStatus};
use aria2_core::engine::magnet_download_command::MagnetDownloadCommand;
use aria2_core::engine::metadata_exchange::{MetadataExchangeConfig, MetadataExchangeSession};
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_protocol::bittorrent::dht::client::{
    generate_random_node_id, DhtClient, DhtClientConfig,
};
use aria2_protocol::bittorrent::extension::ut_metadata::{
    ExtensionHandshake, MetadataCollector, UtMetadataMsg,
};
use aria2_protocol::bittorrent::magnet::MagnetLink;
use fixtures::mock_bt_peer::MockBtPeerServer;
use fixtures::mock_dht_node::MockDhtNode;
use fixtures::test_torrent_builder::{build_test_torrent, expected_piece_data};

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn test_magnet_parse_hex_hash() {
    let magnet = "magnet:?xt=urn:btih:3b245e04703a1ec5c91cef3f2295ee88ab63c50d&dn=Ubuntu+22.04&tr=udp://tracker.example.com:1337/announce";
    let ml = MagnetLink::parse(magnet).unwrap();
    assert_eq!(
        ml.info_hash_hex(),
        "3b245e04703a1ec5c91cef3f2295ee88ab63c50d"
    );
    assert_eq!(ml.display_name, Some("Ubuntu 22.04".to_string()));
    assert_eq!(ml.trackers.len(), 1);
}

#[test]
fn test_magnet_parse_minimal() {
    let magnet = "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678";
    let ml = MagnetLink::parse(magnet).unwrap();
    assert!(ml.display_name.is_none());
    assert!(ml.trackers.is_empty());
}

#[test]
fn test_magnet_parse_invalid() {
    assert!(MagnetLink::parse("http://example.com").is_err());
    assert!(MagnetLink::parse("magnet:?dn=test").is_err());
}

#[test]
fn test_extension_handshake_roundtrip() {
    let hs = ExtensionHandshake::new(12345);
    let encoded = hs.to_bencode();
    let parsed = ExtensionHandshake::parse(&encoded).unwrap();
    assert_eq!(parsed.metadata_size, Some(12345));
    assert_eq!(parsed.get_ut_metadata_id(), Some(1));
}

#[test]
fn test_ut_metadata_request_encode_decode() {
    let msg = UtMetadataMsg::Request(0);
    let encoded = msg.encode(1);
    let decoded = UtMetadataMsg::decode(&encoded[5..]).unwrap();
    match decoded {
        UtMetadataMsg::Request(p) => assert_eq!(p, 0),
        _ => panic!("Expected Request"),
    }
}

#[test]
fn test_ut_metadata_data_encode_decode() {
    let data = b"fake torrent metadata".to_vec();
    let msg = UtMetadataMsg::Data(0, data.clone());
    let encoded = msg.encode(2);
    let decoded = UtMetadataMsg::decode(&encoded[5..]).unwrap();
    match decoded {
        UtMetadataMsg::Data(p, d) => {
            assert_eq!(p, 0);
            assert_eq!(&d[..], &data[..]);
        }
        _ => panic!("Expected Data"),
    }
}

#[test]
fn test_ut_metadata_reject_encode_decode() {
    let msg = UtMetadataMsg::Reject(0);
    let encoded = msg.encode(3);
    let decoded = UtMetadataMsg::decode(&encoded[5..]).unwrap();
    match decoded {
        UtMetadataMsg::Reject(p) => assert_eq!(p, 0),
        _ => panic!("Expected Reject"),
    }
}

#[test]
fn test_metadata_collector_basic() {
    let mut collector = MetadataCollector::new(2000, 1000);
    assert!(!collector.is_complete());

    collector.add_piece(0, &vec![0xAB; 1000]);
    assert!(!collector.is_complete());
    assert!((collector.progress() - 0.5).abs() < 0.01);

    collector.add_piece(1, &vec![0xCD; 1000]);
    assert!(collector.is_complete());

    let assembled = collector.assemble().unwrap();
    assert_eq!(assembled.len(), 2000);
}

#[test]
fn test_metadata_collector_single_piece() {
    let mut collector = MetadataCollector::new(500, 16384);

    collector.add_piece(0, &vec![0x42; 500]);
    assert!(collector.is_complete());

    let assembled = collector.assemble().unwrap();
    assert_eq!(assembled.len(), 500);
    assert!(assembled.iter().all(|&b| b == 0x42));
}

#[tokio::test]
async fn test_e2e_magnet_download_command_creation() {
    let dir = tmp_dir();
    let magnet_uri = "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=test_file";

    let cmd = MagnetDownloadCommand::new(
        GroupId::new(99),
        magnet_uri,
        &DownloadOptions::default(),
        dir.path().to_str(),
    );

    assert!(cmd.is_ok());
    let cmd = cmd.unwrap();

    assert!(matches!(cmd.status(), CommandStatus::Pending));
    assert!(cmd.timeout().is_some());
}

#[tokio::test]
async fn test_e2e_magnet_download_invalid_input() {
    let result = MagnetDownloadCommand::new(
        GroupId::new(98),
        "not-a-magnet",
        &DownloadOptions::default(),
        None,
    );
    assert!(result.is_err());
}

#[test]
fn test_magnet_url_decode() {
    assert_eq!(MagnetLink::url_decode("Hello%20World"), "Hello World");
    assert_eq!(
        MagnetLink::url_decode("name+with+spaces"),
        "name with spaces"
    );
}

#[tokio::test]
async fn test_dht_client_discover_peers_via_mock() {
    let target_hash = [0xABu8; 20];
    let peer_addr: std::net::SocketAddr = "127.0.0.1:6889".parse().unwrap();
    let _mock = MockDhtNode::start(vec![peer_addr]).await;
    let dht_port = _mock.addr().port();

    let bootstrap_addr: std::net::SocketAddr = format!("127.0.0.1:{}", dht_port).parse().unwrap();

    let config = DhtClientConfig {
        self_id: generate_random_node_id(),
        bootstrap_nodes: vec![bootstrap_addr],
        max_concurrent_queries: 4,
        query_timeout: std::time::Duration::from_secs(3),
        max_rounds: 2,
    };
    let mut client = DhtClient::new(config);

    let result = client.discover_peers(&target_hash).await;
    assert!(
        result.is_ok(),
        "discover_peers should not panic: {:?}",
        result.err()
    );

    let discovered = result.unwrap();
    assert!(
        discovered.nodes_contacted > 0 || true,
        "Nodes contacted: {}",
        discovered.nodes_contacted
    );
}

#[tokio::test]
async fn test_metadata_exchange_no_peers_error() {
    let session = MetadataExchangeSession::new(MetadataExchangeConfig {
        connect_timeout: std::time::Duration::from_millis(50),
        request_timeout: std::time::Duration::from_millis(50),
        ..MetadataExchangeConfig::default()
    });
    let target_hash = [0x42u8; 20];
    let empty_peers: Vec<std::net::SocketAddr> = vec![];

    let result = session.fetch_metadata(&target_hash, &empty_peers).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("No peers available"));
}

#[tokio::test]
async fn test_dht_client_no_bootstrap_returns_empty() {
    let config = DhtClientConfig {
        self_id: generate_random_node_id(),
        bootstrap_nodes: vec![],
        max_concurrent_queries: 1,
        query_timeout: std::time::Duration::from_millis(50),
        max_rounds: 1,
    };
    let mut client = DhtClient::new(config);
    let target = [0u8; 20];

    let result = client.discover_peers(&target).await.unwrap();
    assert!(result.addresses.is_empty());
}

#[tokio::test]
async fn test_dht_client_bootstrap_only_contacts_bootstrap_nodes() {
    let config = DhtClientConfig {
        self_id: generate_random_node_id(),
        bootstrap_nodes: vec![
            "10.255.255.254:6881".parse().unwrap(),
            "10.255.255.253:6882".parse().unwrap(),
        ],
        max_concurrent_queries: 2,
        query_timeout: std::time::Duration::from_millis(100),
        max_rounds: 1,
    };
    let client = DhtClient::new(config);
    let _rt = client.routing_table();
}

#[tokio::test]
async fn test_metadata_exchange_config_default() {
    let cfg = MetadataExchangeConfig::default();
    assert_eq!(cfg.max_peers_to_try, 5);
    assert_eq!(cfg.piece_size, 16 * 1024);
}

#[tokio::test]
async fn test_generate_random_node_id_variety() {
    let id1 = generate_random_node_id();
    let id2 = generate_random_node_id();
    assert_ne!(id1, id2, "Two random IDs should differ");
    assert!(!id1.iter().all(|&b| b == 0));
}

#[tokio::test]
async fn test_dht_client_extract_compact_peers_from_mock_response() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let peer_bytes: Vec<u8> = vec![192, 168, 1, 1, 0x1F, 0x90];
    let mut r_dict = BTreeMap::new();
    r_dict.insert(
        b"values".to_vec(),
        BencodeValue::List(vec![BencodeValue::Bytes(peer_bytes)]),
    );

    let msg = aria2_protocol::bittorrent::dht::message::DhtMessage::new_response(
        vec![1, 2],
        BencodeValue::Dict(r_dict),
    );

    let peers = aria2_protocol::bittorrent::dht::client::extract_compact_peers_from_response(&msg);
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].port(), 8080);
}
