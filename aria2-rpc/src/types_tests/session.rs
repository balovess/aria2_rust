//! Global and session DTO tests.

use crate::types::*;

#[test]
fn test_global_stat_default() {
    let stat = GlobalStat::default();
    assert_eq!(stat.download_speed, 0);
    let val = stat.to_json_value();
    assert!(val.get("downloadSpeed").is_some());
}

#[test]
fn test_generate_gid() {
    let gid1 = create_gid();
    let gid2 = create_gid();
    assert_eq!(gid1.len(), 16);
    assert_ne!(gid1, gid2);
}

#[test]
fn test_global_stat_numeric_fields_are_strings() {
    let stat = GlobalStat {
        download_speed: 1024000,
        upload_speed: 51200,
        num_active: 2,
        num_waiting: 3,
        num_stopped: 1,
        num_stopped_total: 1,
    };
    let json = stat.to_json_value();
    assert_eq!(json["downloadSpeed"].as_str(), Some("1024000"));
    assert_eq!(json["uploadSpeed"].as_str(), Some("51200"));
    assert_eq!(json["numActive"].as_str(), Some("2"));
    assert_eq!(json["numWaiting"].as_str(), Some("3"));
    assert_eq!(json["numStopped"].as_str(), Some("1"));
    assert_eq!(json["numStoppedTotal"].as_str(), Some("1"));
}

#[test]
fn test_global_stat_roundtrip_uses_wire_strings() {
    let stat: GlobalStat = serde_json::from_value(serde_json::json!({
        "downloadSpeed": "100",
        "uploadSpeed": 20,
        "numActive": "1",
        "numWaiting": 2,
        "numStopped": "3",
        "numStoppedTotal": 4
    }))
    .unwrap();
    assert_eq!(stat.download_speed, 100);
    assert_eq!(stat.upload_speed, 20);
    assert_eq!(stat.num_stopped_total, 4);
    let encoded = serde_json::to_value(stat).unwrap();
    assert_eq!(encoded["downloadSpeed"], "100");
    assert_eq!(encoded["numStoppedTotal"], "4");
}
