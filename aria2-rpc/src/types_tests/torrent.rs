//! Torrent fixture and BitTorrent metadata projection tests.

use crate::types::*;

// -------------------------------------------------------------------------
// Mock torrent builder helpers (self-contained, no external crate dependency)
// -------------------------------------------------------------------------

/// Minimal bencode integer encoding.
fn ben_int(v: i64) -> Vec<u8> {
    format!("i{}e", v).into_bytes()
}

/// Minimal bencode string encoding.
fn ben_str(s: &str) -> Vec<u8> {
    format!("{}:{}", s.len(), s).into_bytes()
}

/// Minimal bencode bytes encoding.
fn ben_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", data.len()).into_bytes();
    out.extend_from_slice(data);
    out
}

/// Minimal bencode dict encoding from a list of (key_bytes, value_bytes).
fn ben_dict(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"d".to_vec();
    for (k, v) in entries {
        out.extend_from_slice(k);
        out.extend_from_slice(v);
    }
    out.push(b'e');
    out
}

/// Minimal bencode list encoding from a list of value bytes.
fn ben_list(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"l".to_vec();
    for item in items {
        out.extend_from_slice(item);
    }
    out.push(b'e');
    out
}

/// Build a mock .torrent file in bencode format with full metadata.
///
/// Metadata included:
/// - announce, announce-list (multi-tier)
/// - comment, creation date, created by
/// - info dict with name, piece length, pieces, length (single-file mode)
fn build_mock_torrent_with_full_metadata() -> Vec<u8> {
    // Pieces: 2 pieces x 20 bytes = 40 bytes of SHA-1 hash data
    let pieces: Vec<u8> = (0..40).map(|i| i as u8).collect();

    let info_dict = ben_dict(&[
        (ben_str("name"), ben_str("test-file.iso")),
        (ben_str("length"), ben_int(1048576)),      // 1 MiB
        (ben_str("piece length"), ben_int(262144)), // 256 KiB
        (ben_str("pieces"), ben_bytes(&pieces)),
    ]);

    // announce-list: [[tier1_uri], [tier2_uri_a, tier2_uri_b]]
    let tier1 = ben_list(&[ben_str("udp://tracker.aria2.org:80")]);
    let tier2 = ben_list(&[
        ben_str("http://tracker.example.com:80/announce"),
        ben_str("https://tracker.example.org:443/announce"),
    ]);
    let announce_list = ben_list(&[tier1, tier2]);

    ben_dict(&[
        (
            ben_str("announce"),
            ben_str("http://tracker.example.com:80/announce"),
        ),
        (ben_str("announce-list"), announce_list),
        (
            ben_str("comment"),
            ben_str("Aria2 Rust mock torrent for testing"),
        ),
        (ben_str("creation date"), ben_int(1700000000)),
        (
            ben_str("created by"),
            ben_str(concat!("aria2-rust-test/", env!("CARGO_PKG_VERSION"))),
        ),
        (ben_str("info"), info_dict),
    ])
}

/// Build a mock multi-file torrent with metadata.
fn build_mock_multi_file_torrent() -> Vec<u8> {
    let pieces: Vec<u8> = (0..60).map(|i| i as u8).collect(); // 3 pieces

    let file1_dict = ben_dict(&[
        (ben_str("length"), ben_int(500)),
        (
            ben_str("path"),
            ben_list(&[ben_str("dir1"), ben_str("file1.txt")]),
        ),
    ]);
    let file2_dict = ben_dict(&[
        (ben_str("length"), ben_int(524)),
        (
            ben_str("path"),
            ben_list(&[ben_str("dir2"), ben_str("file2.dat")]),
        ),
    ]);

    let info_dict = ben_dict(&[
        (ben_str("name"), ben_str("multi-dir-torrent")),
        (ben_str("files"), ben_list(&[file1_dict, file2_dict])),
        (ben_str("piece length"), ben_int(512)),
        (ben_str("pieces"), ben_bytes(&pieces)),
    ]);

    let announce_list = ben_list(&[ben_list(&[ben_str("udp://tracker.multi.com:80")])]);

    ben_dict(&[
        (
            ben_str("announce"),
            ben_str("http://tracker.multi.com:80/announce"),
        ),
        (ben_str("announce-list"), announce_list),
        (
            ben_str("comment"),
            ben_str("Multi-file torrent for testing"),
        ),
        (ben_str("creation date"), ben_int(1800000000)),
        (ben_str("created by"), ben_str("aria2-rust-test")),
        (ben_str("info"), info_dict),
    ])
}

// -------------------------------------------------------------------------
// Inline bencode field extractors (minimal, for test use only)
// -------------------------------------------------------------------------

/// Extract a bencode string value by key from a bencode dict.
fn extract_bencode_str(data: &[u8], key: &str) -> Option<String> {
    let key_bytes = key.as_bytes();
    // Search for "<len(key)>:<key>" in the bytes
    let search = format!("{}:{}", key_bytes.len(), key);
    let needle = search.as_bytes();
    let pos = data.windows(needle.len()).position(|w| w == needle)?;
    let value_start = pos + needle.len();
    // Read the length-prefixed string at value_start
    let colon_pos = data[value_start..].iter().position(|&b| b == b':')?;
    let len_str = std::str::from_utf8(&data[value_start..value_start + colon_pos]).ok()?;
    let len: usize = len_str.parse().ok()?;
    let val_start = value_start + colon_pos + 1;
    if val_start + len > data.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&data[val_start..val_start + len]).to_string())
}

/// Extract a bencode integer value by key from a bencode dict.
fn extract_bencode_int(data: &[u8], key: &str) -> Option<i64> {
    let key_bytes = key.as_bytes();
    let search = format!("{}:{}", key_bytes.len(), key);
    let needle = search.as_bytes();
    let pos = data.windows(needle.len()).position(|w| w == needle)?;
    let value_start = pos + needle.len();
    if data[value_start] != b'i' {
        return None;
    }
    let end = data[value_start..].iter().position(|&b| b == b'e')?;
    let int_str = std::str::from_utf8(&data[value_start + 1..value_start + end]).ok()?;
    int_str.parse().ok()
}

/// Parse announce-list from bencoded data.
fn parse_announce_list_from_bytes(data: &[u8]) -> Vec<Vec<String>> {
    let search = b"13:announce-list";
    let pos = data.windows(search.len()).position(|w| w == search);
    let start = match pos {
        Some(p) => p + search.len(),
        None => return Vec::new(),
    };

    // The value at `start` should be a bencode list 'l'
    if start >= data.len() || data[start] != b'l' {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut i = start + 1; // skip the outer 'l'
    let data_len = data.len();

    while i < data_len && data[i] != b'e' {
        if data[i] != b'l' {
            break; // expect tier list
        }
        i += 1; // skip tier 'l'
        let mut tier = Vec::new();
        while i < data_len && data[i] != b'e' {
            // Expect a bencode string
            let colon_pos = match data[i..].iter().position(|&b| b == b':') {
                Some(p) => p,
                None => return result,
            };
            let len_str = match std::str::from_utf8(&data[i..i + colon_pos]) {
                Ok(s) => s,
                Err(_) => return result,
            };
            let len: usize = match len_str.parse() {
                Ok(n) => n,
                Err(_) => return result,
            };
            let val_start = i + colon_pos + 1;
            if val_start + len > data_len {
                return result;
            }
            let url = String::from_utf8_lossy(&data[val_start..val_start + len]).to_string();
            tier.push(url);
            i = val_start + len;
        }
        if data[i] == b'e' {
            i += 1; // skip tier 'e'
        }
        if !tier.is_empty() {
            result.push(tier);
        }
    }
    result
}

// -------------------------------------------------------------------------
// BittorrentInfo construction test
// -------------------------------------------------------------------------

/// Construct BittorrentInfo from raw bencoded torrent bytes.
///
/// This simulates what a future `BittorrentInfo::from_bytes()` or the
/// RPC engine's torrent → BittorrentInfo conversion would do.
fn bittorrent_info_from_torrent_bytes(data: &[u8]) -> BittorrentInfo {
    let announce_list = parse_announce_list_from_bytes(data);
    let comment = extract_bencode_str(data, "comment");
    let creation_date = extract_bencode_int(data, "creation date");
    let name = extract_bencode_str(data, "name").unwrap_or_default();

    // Determine mode: search for "5:files" key in the root dict.
    // Multi-file torrents have a "files" key inside the info dict;
    // single-file torrents use "length" instead.
    let mode = if data.windows(7).any(|w| w == b"5:files") {
        Some("multi".to_string())
    } else {
        Some("single".to_string())
    };

    BittorrentInfo {
        announce_list,
        comment,
        creation_date,
        mode,
        info: Some(BittorrentMetaInfo { name }),
    }
}

#[test]
fn test_bittorrent_info_from_mock_single_file_torrent() {
    let torrent_bytes = build_mock_torrent_with_full_metadata();
    let bt_info = bittorrent_info_from_torrent_bytes(&torrent_bytes);

    // --- Verify BittorrentInfo field values ---
    assert_eq!(
        bt_info.announce_list.len(),
        2,
        "announce-list should have 2 tiers"
    );
    assert_eq!(
        bt_info.announce_list[0],
        vec!["udp://tracker.aria2.org:80"],
        "tier 1 should have 1 tracker"
    );
    assert_eq!(
        bt_info.announce_list[1],
        vec![
            "http://tracker.example.com:80/announce",
            "https://tracker.example.org:443/announce",
        ],
        "tier 2 should have 2 trackers"
    );
    assert_eq!(
        bt_info.comment.as_deref(),
        Some("Aria2 Rust mock torrent for testing"),
        "comment should match"
    );
    assert_eq!(
        bt_info.creation_date,
        Some(1700000000),
        "creationDate should be 1700000000"
    );
    assert_eq!(
        bt_info.mode.as_deref(),
        Some("single"),
        "mode should be 'single' for single-file torrent"
    );
    assert_eq!(
        bt_info.info.as_ref().unwrap().name,
        "test-file.iso",
        "info.name should match"
    );

    // --- Verify JSON serialization matches original aria2 format ---
    let json = serde_json::to_value(&bt_info).unwrap();

    // announceList: array of arrays of strings
    let announce_list = json["announceList"].as_array().unwrap();
    assert_eq!(announce_list.len(), 2);
    assert_eq!(announce_list[0][0], "udp://tracker.aria2.org:80");
    assert_eq!(
        announce_list[1][0],
        "http://tracker.example.com:80/announce"
    );
    assert_eq!(
        announce_list[1][1],
        "https://tracker.example.org:443/announce"
    );

    // comment (string)
    assert_eq!(json["comment"], "Aria2 Rust mock torrent for testing");

    // creationDate (integer)
    assert_eq!(json["creationDate"], 1700000000);
    assert!(
        json["creationDate"].is_number(),
        "creationDate should be a JSON number in original aria2"
    );

    // mode (string)
    assert_eq!(json["mode"], "single");

    // info.name
    assert_eq!(json["info"]["name"], "test-file.iso");
}

#[test]
fn test_bittorrent_info_from_mock_multi_file_torrent() {
    let torrent_bytes = build_mock_multi_file_torrent();
    let bt_info = bittorrent_info_from_torrent_bytes(&torrent_bytes);

    assert_eq!(
        bt_info.announce_list,
        vec![vec!["udp://tracker.multi.com:80"]],
        "announce-list should have 1 tier with 1 tracker"
    );
    assert_eq!(
        bt_info.comment.as_deref(),
        Some("Multi-file torrent for testing"),
        "comment should match"
    );
    assert_eq!(
        bt_info.creation_date,
        Some(1800000000),
        "creationDate should be 1800000000"
    );
    assert_eq!(
        bt_info.mode.as_deref(),
        Some("multi"),
        "mode should be 'multi' for multi-file torrent"
    );
    assert_eq!(
        bt_info.info.as_ref().unwrap().name,
        "multi-dir-torrent",
        "info.name should be the top-level dir name"
    );

    // JSON verification
    let json = serde_json::to_value(&bt_info).unwrap();
    assert_eq!(json["mode"], "multi");
    assert_eq!(json["info"]["name"], "multi-dir-torrent");
    assert_eq!(json["creationDate"], 1800000000);
}

#[test]
fn test_bittorrent_info_minimal_fields() {
    // A torrent with only the basics (like a magnet-based torrent with no metadata)
    let bt = BittorrentInfo {
        announce_list: vec![],
        comment: None,
        creation_date: None,
        mode: Some("single".to_string()),
        info: Some(BittorrentMetaInfo {
            name: "unknown.torrent".to_string(),
        }),
    };

    let json = serde_json::to_value(&bt).unwrap();
    // Fields with None should be skipped
    assert!(json.get("comment").is_none(), "comment should be omitted");
    assert!(
        json.get("creationDate").is_none(),
        "creationDate should be omitted"
    );
    assert_eq!(json["mode"], "single");
    assert_eq!(json["info"]["name"], "unknown.torrent");
    // Empty announce_list should still appear (Vec is not Option)
    assert_eq!(
        json["announceList"].as_array().unwrap().len(),
        0,
        "announce-list should be empty array"
    );
}

#[test]
fn test_tell_status_with_bittorrent_from_mock_torrent() {
    let torrent_bytes = build_mock_torrent_with_full_metadata();
    let bt_info = bittorrent_info_from_torrent_bytes(&torrent_bytes);

    let status = StatusInfo::new("bt-mock-gid-001")
        .with_total_length(1048576)
        .with_completed_length(0)
        .with_download_speed(0)
        .with_upload_speed(0)
        .with_status(DownloadStatus::Active)
        .with_dir("/downloads/aria2")
        .with_bittorrent(bt_info)
        .with_following("child-gid-001");

    let json = serde_json::to_value(&status).unwrap();

    // Verify top-level fields
    assert_eq!(json["gid"], "bt-mock-gid-001");
    assert_eq!(json["status"], "active");
    assert_eq!(json["dir"], "/downloads/aria2");
    assert_eq!(json["following"], "child-gid-001");

    // Verify nested bittorrent object
    let bt_json = json.get("bittorrent").unwrap();
    assert_eq!(bt_json["announceList"][0][0], "udp://tracker.aria2.org:80");
    assert_eq!(bt_json["comment"], "Aria2 Rust mock torrent for testing");
    assert_eq!(bt_json["creationDate"], 1700000000);
    assert_eq!(bt_json["mode"], "single");
    assert_eq!(bt_json["info"]["name"], "test-file.iso");

    // Verify the JSON structure matches original aria2 tellStatus output
    // (bittorrent as a nested object, not at top level)
    let json_str = serde_json::to_string_pretty(&json).unwrap();
    assert!(
        json_str.contains("\"bittorrent\""),
        "JSON should contain bittorrent key"
    );
    assert!(
        json_str.contains("\"announceList\""),
        "JSON should contain announceList inside bittorrent"
    );
    assert!(
        json_str.contains("\"Aria2 Rust mock torrent for testing\""),
        "JSON should contain the comment text"
    );
}

#[test]
fn test_mock_torrent_builder_roundtrip() {
    let torrent_bytes = build_mock_torrent_with_full_metadata();

    // Verify the bencoded data starts with 'd' (dict) and ends with 'e'
    assert_eq!(
        torrent_bytes.first(),
        Some(&b'd'),
        "Bencoded torrent should start with 'd'"
    );
    assert_eq!(
        torrent_bytes.last(),
        Some(&b'e'),
        "Bencoded torrent should end with 'e'"
    );

    // Verify the bencoded data contains expected key prefixes
    let as_text = String::from_utf8_lossy(&torrent_bytes);
    assert!(
        as_text.contains("8:announce"),
        "Should contain announce key"
    );
    assert!(
        as_text.contains("13:announce-list"),
        "Should contain announce-list key"
    );
    assert!(as_text.contains("7:comment"), "Should contain comment key");
    assert!(
        as_text.contains("13:creation date"),
        "Should contain creation date key"
    );
    assert!(as_text.contains("4:info"), "Should contain info key");
    assert!(as_text.contains("4:name"), "Should contain name key");

    // Verify we can parse back the extracted fields
    assert_eq!(
        extract_bencode_str(&torrent_bytes, "comment"),
        Some("Aria2 Rust mock torrent for testing".to_string())
    );
    assert_eq!(
        extract_bencode_int(&torrent_bytes, "creation date"),
        Some(1700000000)
    );
    assert_eq!(
        extract_bencode_str(&torrent_bytes, "name"),
        Some("test-file.iso".to_string())
    );

    // Verify announce-list parsing
    let announce_list = parse_announce_list_from_bytes(&torrent_bytes);
    assert_eq!(announce_list.len(), 2);
    assert_eq!(announce_list[0], vec!["udp://tracker.aria2.org:80"]);
    assert_eq!(
        announce_list[1],
        vec![
            "http://tracker.example.com:80/announce",
            "https://tracker.example.org:443/announce",
        ]
    );
}

#[test]
fn test_empty_announce_list() {
    // Build a torrent without announce-list
    let pieces: Vec<u8> = (0..20).map(|i| i as u8).collect();
    let info_dict = ben_dict(&[
        (ben_str("name"), ben_str("no-tracker.torrent")),
        (ben_str("length"), ben_int(1024)),
        (ben_str("piece length"), ben_int(512)),
        (ben_str("pieces"), ben_bytes(&pieces)),
    ]);
    let torrent = ben_dict(&[
        (ben_str("announce"), ben_str("http://example.com/announce")),
        (ben_str("info"), info_dict),
    ]);

    let announce_list = parse_announce_list_from_bytes(&torrent);
    assert!(
        announce_list.is_empty(),
        "announce-list should be empty when not present in torrent"
    );

    let bt_info = bittorrent_info_from_torrent_bytes(&torrent);
    assert!(
        bt_info.announce_list.is_empty(),
        "BittorrentInfo.announce_list should be empty"
    );
    assert!(bt_info.comment.is_none(), "comment should be None");
    assert!(
        bt_info.creation_date.is_none(),
        "creationDate should be None"
    );
    assert_eq!(
        bt_info.mode.as_deref(),
        Some("single"),
        "mode should be 'single'"
    );
}
