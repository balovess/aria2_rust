use super::*;

#[test]
fn response_header_decodes_common_fields_and_preserves_unknown_actions() {
    let mut data = [0u8; 8];
    data[..4].copy_from_slice(&99i32.to_be_bytes());
    data[4..].copy_from_slice(&0x1234_5678u32.to_be_bytes());

    assert_eq!(
        decode_response_header(&data),
        Some(UdpTrackerResponseHeader {
            action: 99,
            transaction_id: 0x1234_5678,
        })
    );
    assert_eq!(decode_response_header(&data[..7]), None);
}

#[test]
fn test_build_connect_request_length_and_magic() {
    let req = build_connect_request(0x12345678);
    assert_eq!(req.len(), 16);

    let conn_id = u64::from_be_bytes([
        req[0], req[1], req[2], req[3], req[4], req[5], req[6], req[7],
    ]);
    assert_eq!(conn_id, INITIAL_CONNECTION_ID);

    let action = i32::from_be_bytes([req[8], req[9], req[10], req[11]]);
    assert_eq!(action, 0);

    let txn = u32::from_be_bytes([req[12], req[13], req[14], req[15]]);
    assert_eq!(txn, 0x12345678);
}

#[test]
fn test_build_announce_request_format() {
    let info_hash = [0xABu8; 20];
    let peer_id = [0xCDu8; 20];
    let req = build_announce_request(
        0x123456789ABCDEF0,
        0xDEADBEEF,
        &info_hash,
        &peer_id,
        1024,
        2048,
        4096,
        UdpEvent::Started,
        0,
        0x12345678,
        50,
        6881,
    );
    assert_eq!(req.len(), 98);

    let conn_id = u64::from_be_bytes(req[..8].try_into().unwrap());
    assert_eq!(conn_id, 0x123456789ABCDEF0);

    let event = i32::from_be_bytes(req[80..84].try_into().unwrap());
    assert_eq!(event, UdpEvent::Started as i32);

    let port = u16::from_be_bytes(req[96..98].try_into().unwrap());
    assert_eq!(port, 6881);
}

#[test]
fn test_build_announce_request_uses_bep15_counter_order() {
    let downloaded = 0x0102_0304_0506_0708i64;
    let left = 0x1112_1314_1516_1718i64;
    let uploaded = 0x2122_2324_2526_2728i64;
    let request = build_announce_request(
        0x123456789ABCDEF0,
        0xDEADBEEF,
        &[0xAB; 20],
        &[0xCD; 20],
        downloaded,
        left,
        uploaded,
        UdpEvent::Started,
        0,
        0x12345678,
        50,
        6881,
    );

    assert_eq!(&request[56..64], &downloaded.to_be_bytes());
    assert_eq!(&request[64..72], &left.to_be_bytes());
    assert_eq!(&request[72..80], &uploaded.to_be_bytes());
}

#[test]
fn test_parse_connect_response_valid() {
    let mut data = vec![0u8; 16];
    data[0..4].copy_from_slice(&(0i32).to_be_bytes()); // action=connect
    data[4..8].copy_from_slice(&0xABCDEF01u32.to_be_bytes()); // txn_id
    data[8..16].copy_from_slice(&0x123456789ABCDEF0u64.to_be_bytes()); // connection_id

    let resp = parse_connect_response(&data).unwrap();
    assert_eq!(resp.transaction_id, 0xABCDEF01);
    assert_eq!(resp.connection_id, 0x123456789ABCDEF0);
}

#[test]
fn test_parse_connect_response_too_short() {
    assert!(parse_connect_response(&[0u8; 15]).is_err());
    assert!(parse_connect_response(&[]).is_err());
}

#[test]
fn test_parse_connect_response_rejects_trailing_bytes() {
    assert!(parse_connect_response(&[0u8; 17]).is_err());
}

#[test]
fn test_parse_connect_response_wrong_action() {
    let mut data = vec![0u8; 16];
    data[0..4].copy_from_slice(&3i32.to_be_bytes()); // action=error
    assert!(parse_connect_response(&data).is_err());
}

#[test]
fn test_parse_announce_with_peers() {
    let mut data = vec![0u8; 20];
    data[0..4].copy_from_slice(&1i32.to_be_bytes()); // action=announce
    data[4..8].copy_from_slice(&0x12345678u32.to_be_bytes()); // txn_id
    data[8..12].copy_from_slice(&900u32.to_be_bytes()); // interval=900s
    data[12..16].copy_from_slice(&5u32.to_be_bytes()); // leechers
    data[16..20].copy_from_slice(&3u32.to_be_bytes()); // seeders

    data.extend_from_slice(&[
        192, 168, 1, 1, 0x19, 0xFA, // 192.168.1.1:6650
        10, 0, 0, 1, 0x17, 0x70, // 10.0.0.1:6000
    ]);

    let resp = parse_announce_response(&data).unwrap();
    assert_eq!(resp.transaction_id, 0x12345678);
    assert_eq!(resp.interval, 900);
    assert_eq!(resp.leechers, 5);
    assert_eq!(resp.seeders, 3);
    assert_eq!(resp.peers.len(), 2);
    assert_eq!(resp.peers[0], ("192.168.1.1".into(), 6650));
    assert_eq!(resp.peers[1], ("10.0.0.1".into(), 6000));
}

#[test]
fn test_parse_announce_empty_peers() {
    let mut data = vec![0u8; 20];
    data[0..4].copy_from_slice(&1i32.to_be_bytes());
    data[4..8].copy_from_slice(&0u32.to_be_bytes());
    data[8..12].copy_from_slice(&1800u32.to_be_bytes());

    let resp = parse_announce_response(&data).unwrap();
    assert_eq!(resp.interval, 1800);
    assert!(resp.peers.is_empty());
}

#[test]
fn test_parse_announce_too_short() {
    assert!(parse_announce_response(&[0u8; 19]).is_err());
}

#[test]
fn test_parse_announce_rejects_partial_compact_peer() {
    let mut data = vec![0u8; 21];
    data[0..4].copy_from_slice(&(UdpAction::Announce as i32).to_be_bytes());

    let result = parse_announce_response(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not a multiple of 6"));
}

#[test]
fn test_parse_announce_error_action() {
    let mut data = vec![0u8; 23];
    data[0..4].copy_from_slice(&3i32.to_be_bytes()); // error
    data[8..23].copy_from_slice(b"tracker offline");

    let result = parse_announce_response(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("tracker offline"));
}

#[test]
fn test_parse_short_announce_error_response() {
    let mut data = vec![0u8; 8];
    data[0..4].copy_from_slice(&(UdpAction::Error as i32).to_be_bytes());
    data.extend_from_slice(b"offline");

    let result = parse_announce_response(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("offline"));
}

#[test]
fn test_parse_announce_rejects_non_announce_action() {
    let mut data = vec![0u8; 20];
    data[0..4].copy_from_slice(&(UdpAction::Connect as i32).to_be_bytes());

    let result = parse_announce_response(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unexpected action"));
}

#[test]
fn test_parse_compact_peer_format() {
    let peer_data = [
        127, 0, 0, 1, 0x1F, 0x90, // 127.0.0.1:8080
        255, 255, 255, 255, 0x00, 0x50, // 255.255.255.255:80
        10, 0, 0, 1, 0xBB, 0x82, // 10.0.0.1:48002
    ];
    let mut response = vec![0; 20];
    response[0..4].copy_from_slice(&(UdpAction::Announce as i32).to_be_bytes());
    response.extend_from_slice(&peer_data);

    let parsed = parse_announce_response(&response).unwrap();
    assert_eq!(parsed.peers.len(), 3);
    assert_eq!(parsed.peers[0], ("127.0.0.1".into(), 8080));
    assert_eq!(parsed.peers[1], ("255.255.255.255".into(), 80));
    assert_eq!(parsed.peers[2], ("10.0.0.1".into(), 48002));
}

#[test]
fn test_event_enum_values() {
    assert_eq!(UdpEvent::None as i32, 0);
    assert_eq!(UdpEvent::Completed as i32, 1);
    assert_eq!(UdpEvent::Started as i32, 2);
    assert_eq!(UdpEvent::Stopped as i32, 3);
}

#[test]
fn test_big_endian_encoding_consistency() {
    let val: u32 = 0xAABBCCDD;
    let bytes = val.to_be_bytes();
    assert_eq!(bytes, [0xAA, 0xBB, 0xCC, 0xDD]);
    assert_eq!(u32::from_be_bytes(bytes), val);
}

#[test]
fn test_udp_action_roundtrip() {
    for a in &[
        UdpAction::Connect,
        UdpAction::Announce,
        UdpAction::Scrape,
        UdpAction::Error,
    ] {
        assert_eq!(UdpAction::from_i32(*a as i32), Some(*a));
    }
    assert_eq!(UdpAction::from_i32(99), None);
}

// --- Scrape tests ---

#[test]
fn test_scrape_request_encoding() {
    let conn_id = 0x123456789ABCDEF0u64;
    let txn_id = 0xDEADBEEFu32;
    let info_hashes = [[0xABu8; 20], [0xCDu8; 20]];

    let req = build_scrape_request(conn_id, txn_id, &info_hashes);

    // Header: conn_id(8) + action(4) + txn_id(4) + hashes(20*2) = 56 bytes
    assert_eq!(req.len(), 56);

    // Verify connection ID
    let parsed_conn_id = u64::from_be_bytes(req[0..8].try_into().unwrap());
    assert_eq!(parsed_conn_id, conn_id);

    // Verify action = 2 (scrape)
    let action = i32::from_be_bytes(req[8..12].try_into().unwrap());
    assert_eq!(action, UdpAction::Scrape as i32);

    // Verify transaction ID
    let parsed_txn = u32::from_be_bytes(req[12..16].try_into().unwrap());
    assert_eq!(parsed_txn, txn_id);

    // Verify first info hash
    assert_eq!(&req[16..36], &[0xABu8; 20]);

    // Verify second info hash
    assert_eq!(&req[36..56], &[0xCDu8; 20]);
}

#[test]
fn test_scrape_response_parsing_single_hash() {
    let mut data = vec![0u8; 20]; // 8 header + 12 data for 1 hash
    data[0..4].copy_from_slice(&(UdpAction::Scrape as i32).to_be_bytes()); // action=2
    data[4..8].copy_from_slice(&0x12345678u32.to_be_bytes()); // txn_id
    data[8..12].copy_from_slice(&100u32.to_be_bytes()); // seeders=100
    data[12..16].copy_from_slice(&50u32.to_be_bytes()); // leechers=50
    data[16..20].copy_from_slice(&200u32.to_be_bytes()); // completed=200

    let results = parse_scrape_response(&data).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0],
        ScrapeResult {
            seeders: 100,
            leechers: 50,
            completed: 200,
        }
    );
}

#[test]
fn test_scrape_response_parsing_multi_hash() {
    let mut data = vec![0u8; 44]; // 8 header + 12*3 data for 3 hashes
    data[0..4].copy_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
    data[4..8].copy_from_slice(&0xAABBCCDDu32.to_be_bytes());

    // Hash 1: seeders=10, leechers=5, completed=100
    data[8..12].copy_from_slice(&10u32.to_be_bytes());
    data[12..16].copy_from_slice(&5u32.to_be_bytes());
    data[16..20].copy_from_slice(&100u32.to_be_bytes());

    // Hash 2: seeders=999, leechers=888, completed=777
    data[20..24].copy_from_slice(&999u32.to_be_bytes());
    data[24..28].copy_from_slice(&888u32.to_be_bytes());
    data[28..32].copy_from_slice(&777u32.to_be_bytes());

    // Hash 3: seeders=1, leechers=2, completed=3
    data[32..36].copy_from_slice(&1u32.to_be_bytes());
    data[36..40].copy_from_slice(&2u32.to_be_bytes());
    data[40..44].copy_from_slice(&3u32.to_be_bytes());

    let results = parse_scrape_response(&data).unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].seeders, 10);
    assert_eq!(results[0].leechers, 5);
    assert_eq!(results[0].completed, 100);
    assert_eq!(results[1].seeders, 999);
    assert_eq!(results[1].leechers, 888);
    assert_eq!(results[1].completed, 777);
    assert_eq!(results[2].seeders, 1);
    assert_eq!(results[2].leechers, 2);
    assert_eq!(results[2].completed, 3);
}

#[test]
fn test_scrape_error_action() {
    // Action != 2 should return error
    let mut data = vec![0u8; 20];
    data[0..4].copy_from_slice(&3i32.to_be_bytes()); // error action
    data[4..8].copy_from_slice(&0x12345678u32.to_be_bytes());

    let result = parse_scrape_response(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unexpected action"));
}

#[test]
fn test_scrape_response_too_short() {
    // Less than 8 bytes should error
    assert!(parse_scrape_response(&[0u8; 7]).is_err());
    assert!(parse_scrape_response(&[]).is_err());
}
