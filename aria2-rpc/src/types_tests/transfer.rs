//! URI, file, server, and peer DTO tests.

use crate::types::*;

#[test]
fn test_file_info_default() {
    let fi = FileInfo::default();
    assert!(fi.selected);
    assert_eq!(fi.uris.len(), 0);
}

#[test]
fn test_file_info_builder() {
    let fi = FileInfo::new("/tmp/file.iso", 1048576)
        .with_uris(vec![UriEntry::new("http://example.com/file.iso")]);
    assert_eq!(fi.length, 1048576);
    assert_eq!(fi.uris.len(), 1);
}

#[test]
fn test_uri_entry() {
    let uri = UriEntry::new("http://example.com/file.iso").used();
    assert_eq!(uri.status, UriStatus::Used);

    let w = UriEntry::new("http://x.com/f").waiting();
    assert_eq!(w.status, UriStatus::Waiting);
}

#[test]
fn test_peer_info_with_bitfield_seeder() {
    let peer = PeerInfo {
        peer_id: "peer-abc123".to_string(),
        ip: "192.168.1.100".to_string(),
        source: "unknown".to_string(),
        port: 6881,
        bitfield: Some("ff00ff00".to_string()),
        am_choking: false,
        peer_choking: true,
        download_speed: 1048576,
        upload_speed: 512000,
        seeder: Some("true".to_string()),
    };
    let json = serde_json::to_value(&peer).unwrap();
    assert_eq!(json["peerId"], "peer-abc123");
    assert_eq!(json["ip"], "192.168.1.100");
    // port, downloadSpeed, uploadSpeed, amChoking, peerChoking are all
    // serialized as strings matching original aria2c wire format
    assert_eq!(json["port"], "6881");
    assert_eq!(json["bitfield"], "ff00ff00");
    assert_eq!(json["amChoking"], "false");
    assert_eq!(json["peerChoking"], "true");
    assert_eq!(json["downloadSpeed"], "1048576");
    assert_eq!(json["uploadSpeed"], "512000");
    assert_eq!(json["seeder"], "true");

    let roundtrip: PeerInfo = serde_json::from_value(json).unwrap();
    assert_eq!(roundtrip.bitfield, Some("ff00ff00".to_string()));
    assert_eq!(roundtrip.seeder, Some("true".to_string()));
}

#[test]
fn test_file_info_numeric_fields_are_strings() {
    let fi = FileInfo::new("/downloads/file.iso", 104857600)
        .with_completed(52428800)
        .with_index(1);
    let json = serde_json::to_value(&fi).unwrap();
    assert_eq!(
        json["index"].as_str(),
        Some("1"),
        "index must be a string: got {:?}",
        json["index"]
    );
    assert_eq!(
        json["length"].as_str(),
        Some("104857600"),
        "length must be a string: got {:?}",
        json["length"]
    );
    assert_eq!(
        json["completedLength"].as_str(),
        Some("52428800"),
        "completedLength must be a string: got {:?}",
        json["completedLength"]
    );
}

#[test]
fn test_uri_status_hides_internal_spent_state_on_wire() {
    assert_eq!(serde_json::to_value(UriStatus::Spent).unwrap(), "used");
    assert_eq!(
        serde_json::from_value::<UriStatus>(serde_json::json!("spent")).unwrap(),
        UriStatus::Spent
    );
    assert_eq!(
        serde_json::from_value::<UriStatus>(serde_json::json!("used")).unwrap(),
        UriStatus::Used
    );
}
