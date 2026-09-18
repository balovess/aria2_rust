//! Wire-format serialization and deserialization tests.

use crate::types::*;

#[test]
fn test_status_info_numeric_fields_are_strings() {
    let info = StatusInfo::new("wire-format-test")
        .with_status(DownloadStatus::Active)
        .with_total_length(104857600)
        .with_completed_length(52428800)
        .with_upload_length(0)
        .with_download_speed(1024000)
        .with_upload_speed(0)
        .with_connections(5);

    let json = serde_json::to_value(&info).unwrap();

    // All numeric fields must be strings in wire format
    assert_eq!(
        json["totalLength"].as_str(),
        Some("104857600"),
        "totalLength must be a string: got {:?}",
        json["totalLength"]
    );
    assert_eq!(
        json["completedLength"].as_str(),
        Some("52428800"),
        "completedLength must be a string: got {:?}",
        json["completedLength"]
    );
    assert_eq!(
        json["uploadLength"].as_str(),
        Some("0"),
        "uploadLength must be a string: got {:?}",
        json["uploadLength"]
    );
    assert_eq!(
        json["downloadSpeed"].as_str(),
        Some("1024000"),
        "downloadSpeed must be a string: got {:?}",
        json["downloadSpeed"]
    );
    assert_eq!(
        json["uploadSpeed"].as_str(),
        Some("0"),
        "uploadSpeed must be a string: got {:?}",
        json["uploadSpeed"]
    );
    assert_eq!(
        json["connections"].as_str(),
        Some("5"),
        "connections must be a string: got {:?}",
        json["connections"]
    );
}

#[test]
fn test_status_info_deserialization_roundtrip() {
    // Serialize a StatusInfo with numeric fields
    let info = StatusInfo::new("test")
        .with_total_length(104857600)
        .with_completed_length(0);

    let json = serde_json::to_value(&info).unwrap();
    // Verify totalLength is a string
    assert_eq!(json["totalLength"].as_str(), Some("104857600"));

    // Parse back the serialized JSON — should succeed since it was
    // produced by our own serializer
    let roundtrip: StatusInfo = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(roundtrip.total_length, Some(104857600));
    assert_eq!(roundtrip.completed_length, Some(0));
}

#[test]
fn test_status_info_piece_counts_use_wire_strings() {
    let info = StatusInfo::new("bt-test")
        .with_num_pieces(10)
        .with_completed_pieces(9)
        .with_missing_pieces(1);

    let json = serde_json::to_value(&info).unwrap();
    assert_eq!(json["numPieces"].as_str(), Some("10"));
    assert_eq!(json["completedPieces"].as_str(), Some("9"));
    assert_eq!(json["missingPieces"].as_str(), Some("1"));

    let roundtrip: StatusInfo = serde_json::from_value(json).unwrap();
    assert_eq!(roundtrip.completed_pieces, Some(9));
    assert_eq!(roundtrip.missing_pieces, Some(1));
}

#[test]
fn test_wire_codecs_accept_aria2_literals_and_native_values() {
    let wire = serde_json::json!({
        "gid": "wire-test",
        "status": "active",
        "totalLength": "104857600",
        "completedLength": 52428800,
        "connections": "4",
        "pieceLength": 262144,
        "numPieces": "400",
        "numSeeders": 3,
        "files": [{
            "index": "1",
            "path": "/downloads/file.bin",
            "length": 104857600,
            "completedLength": "52428800",
            "selected": "true",
            "uris": [{"uri": "http://example.test/file.bin", "status": "used"}]
        }]
    });

    let info: StatusInfo = serde_json::from_value(wire).unwrap();
    assert_eq!(info.total_length, Some(104857600));
    assert_eq!(info.completed_length, Some(52428800));
    assert_eq!(info.connections, Some(4));
    assert_eq!(info.num_pieces, Some(400));
    assert_eq!(info.num_seeders, Some(3));
    assert_eq!(info.files.as_ref().unwrap()[0].index, 1);
    assert_eq!(
        info.files.as_ref().unwrap()[0].uris[0].status,
        UriStatus::Used
    );

    let encoded = serde_json::to_value(info).unwrap();
    assert_eq!(encoded["completedLength"], "52428800");
    assert_eq!(encoded["connections"], "4");
    assert_eq!(encoded["numPieces"], "400");
    assert_eq!(encoded["numSeeders"], "3");
    assert_eq!(encoded["files"][0]["index"], "1");
    assert_eq!(encoded["files"][0]["selected"], "true");
}
