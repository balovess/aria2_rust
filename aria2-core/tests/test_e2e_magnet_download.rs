mod fixtures {
    pub mod test_torrent_builder;
    pub mod mock_bt_peer;
    pub mod mock_dht_node;
}
use fixtures::test_torrent_builder::{build_test_torrent, expected_piece_data};
use fixtures::mock_bt_peer::MockBtPeerServer;
use aria2_core::engine::magnet_download_command::MagnetDownloadCommand;
use aria2_core::engine::command::{Command, CommandStatus};
use aria2_core::request::request_group::{GroupId, DownloadOptions};
use aria2_protocol::bittorrent::magnet::MagnetLink;
use aria2_protocol::bittorrent::extension::ut_metadata::{
    ExtensionHandshake, UtMetadataMsg, MetadataCollector,
};

fn tmp_dir() -> tempfile::TempDir { tempfile::tempdir().unwrap() }

#[test]
fn test_magnet_parse_hex_hash() {
    let magnet = "magnet:?xt=urn:btih:3b245e04703a1ec5c91cef3f2295ee88ab63c50d&dn=Ubuntu+22.04&tr=udp://tracker.example.com:1337/announce";
    let ml = MagnetLink::parse(magnet).unwrap();
    assert_eq!(ml.info_hash_hex(), "3b245e04703a1ec5c91cef3f2295ee88ab63c50d");
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
    assert_eq!(
        MagnetLink::url_decode("Hello%20World"),
        "Hello World"
    );
    assert_eq!(
        MagnetLink::url_decode("name+with+spaces"),
        "name with spaces"
    );
}
