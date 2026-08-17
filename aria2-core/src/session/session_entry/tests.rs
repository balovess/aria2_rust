//! Unit tests for session_entry module

use std::collections::HashMap;

use crate::request::request_group::{DownloadOptions, FollowMode};
use crate::session::session_entry::{SessionEntry, download_options_to_map};

#[test]
fn test_serialize_single_entry() {
    let entry = SessionEntry::new(0xd270c8a2, vec!["http://example.com/file.zip".to_string()]);
    let text = entry.serialize();
    assert!(
        text.contains("http://example.com/file.zip"),
        "Should contain URI"
    );
    // GID is zero-padded to 16 hex digits for C++ aria2 interop.
    assert!(
        text.contains("GID=00000000d270c8a2"),
        "Should contain zero-padded GID"
    );
}

/// C++ aria2 (`GroupId::toNumericId`) requires the session-file GID to be
/// exactly 16 hex digits and rejects the whole entry otherwise. Regression:
/// the serializer used to emit unpadded `{:x}` (e.g. `GID=1`), producing
/// session files C++ cannot load. The GID must always be zero-padded.
#[test]
fn test_serialize_gid_always_16_hex_digits() {
    let cases = [0u64, 1, 0xd270c8a2, u64::MAX];
    for gid in cases {
        let entry = SessionEntry::new(gid, vec!["http://example.com/f".to_string()]);
        let text = entry.serialize();
        let gid_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("GID="))
            .unwrap_or_else(|| panic!("GID line missing for gid={gid:#x}"));
        let hex = gid_line.trim().trim_start_matches("GID=");
        assert_eq!(
            hex.len(),
            16,
            "GID must be exactly 16 hex digits (got {:?}) for gid={gid:#x}",
            hex
        );
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "GID must be lowercase hex (got {hex:?})"
        );
        assert_eq!(
            u64::from_str_radix(hex, 16).unwrap(),
            gid,
            "GID must round-trip for gid={gid:#x}"
        );
    }

    // Round-trip through the parser: a 16-digit GID must parse back.
    let entry = SessionEntry::new(0xd270c8a2, vec!["http://example.com/f".to_string()]);
    let restored = SessionEntry::deserialize_line(&entry.serialize()).unwrap();
    assert_eq!(restored.gid, 0xd270c8a2);
}

#[test]
fn test_serialize_multiple_entries_roundtrip() {
    let entries = vec![
        SessionEntry::new(1, vec!["http://a.com/1.bin".to_string()]).with_options({
            let mut m = HashMap::new();
            m.insert("split".to_string(), "4".to_string());
            m.insert("dir".to_string(), "/tmp".to_string());
            m
        }),
        SessionEntry::new(
            2,
            vec![
                "ftp://b.com/2.iso".to_string(),
                "http://mirror.b.com/2.iso".to_string(),
            ],
        )
        .paused(),
    ];

    let mut serialized = String::new();
    for e in &entries {
        serialized.push_str(&e.serialize());
        serialized.push('\n');
    }

    // Parse individually using deserialize_line
    let parts: Vec<&str> = serialized.split("\n\n").collect();
    assert!(parts.len() >= 2, "Should have at least 2 entries");

    let entry1 = SessionEntry::deserialize_line(parts[0]).unwrap();
    assert_eq!(entry1.uris.len(), 1);
    assert_eq!(entry1.uris[0], "http://a.com/1.bin");
    assert_eq!(entry1.options.get("split").unwrap(), "4");

    let entry2 = SessionEntry::deserialize_line(parts[1]).unwrap();
    assert_eq!(entry2.uris.len(), 2);
    assert!(entry2.paused);
}

#[test]
fn test_deserialize_empty_file() {
    let entry = SessionEntry::deserialize_line("").unwrap();
    assert!(entry.uris.is_empty());

    let entry = SessionEntry::deserialize_line("\n\n\n").unwrap();
    assert!(entry.uris.is_empty());
}

#[test]
fn test_deserialize_skip_comments_and_blanks() {
    let input = r#"# This is a comment
# Another comment

http://example.com/file
 GID=abc123
 dir=/downloads
"#;
    let entry = SessionEntry::deserialize_line(input).unwrap();
    // Should parse first entry and ignore comments
    assert_eq!(entry.uris.len(), 1);
    assert_eq!(entry.uris[0], "http://example.com/file");
}

#[test]
fn test_deserialize_options_parsing() {
    let input = r#"http://example.com/file.zip
 GID=1
 split=4
 max-connection-per-server=2
 dir=C:\Users\test\Downloads
 out=file.zip
"#;
    let entry = SessionEntry::deserialize_line(input).unwrap();
    assert_eq!(entry.options.get("split").unwrap(), "4");
    assert_eq!(entry.options.get("max-connection-per-server").unwrap(), "2");
    assert_eq!(
        entry.options.get("dir").unwrap(),
        "C:\\Users\\test\\Downloads"
    );
    assert_eq!(entry.options.get("out").unwrap(), "file.zip");
}

#[test]
fn test_pause_flag_serialization() {
    let input = r#"http://example.com/pause.me
 GID=42
 PAUSE=true
"#;
    let entry = SessionEntry::deserialize_line(input).unwrap();
    assert!(entry.paused);

    let text = entry.serialize();
    assert!(text.contains("PAUSE=true"));
}

#[test]
fn test_serialize_tab_separated_uris() {
    let entry = SessionEntry::new(
        99,
        vec![
            "http://mirror1.com/f".to_string(),
            "http://mirror2.com/f".to_string(),
            "http://mirror3.com/f".to_string(),
        ],
    );
    let text = entry.serialize();
    let uri_line = text.lines().next().unwrap();
    assert_eq!(
        uri_line.matches('\t').count(),
        2,
        "3 URIs should have 2 tab separators"
    );
}

// ==================== New Field Tests (Session Persistence Enhancement) ====================

#[test]
fn test_serialize_new_fields() {
    let mut entry = SessionEntry::new(1, vec!["http://example.com/file.bin".to_string()]);
    entry.total_length = 1024 * 1024; // 1MB
    entry.completed_length = 512 * 1024; // 512KB
    entry.upload_length = 1024;
    entry.download_speed = 2048;
    entry.status = "active".to_string();
    entry.error_code = None;

    let text = entry.serialize();

    // Verify new fields appear in output
    assert!(
        text.contains("TOTAL_LENGTH=1048576"),
        "Should contain TOTAL_LENGTH"
    );
    assert!(
        text.contains("COMPLETED_LENGTH=524288"),
        "Should contain COMPLETED_LENGTH"
    );
    assert!(
        text.contains("UPLOAD_LENGTH=1024"),
        "Should contain UPLOAD_LENGTH"
    );
    assert!(
        text.contains("DOWNLOAD_SPEED=2048"),
        "Should contain DOWNLOAD_SPEED"
    );
    assert!(text.contains("STATUS=active"), "Should contain STATUS");
}

#[test]
fn test_deserialize_with_all_fields() {
    let input = r#"http://example.com/bigfile.zip
 GID=1
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=5242880
 UPLOAD_LENGTH=2048
 DOWNLOAD_SPEED=4096
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=5242880
"#;

    let entry = SessionEntry::deserialize_line(input).unwrap();

    assert_eq!(entry.total_length, 10485760);
    assert_eq!(entry.completed_length, 5242880);
    assert_eq!(entry.upload_length, 2048);
    assert_eq!(entry.download_speed, 4096);
    assert_eq!(entry.status, "active");
    assert_eq!(entry.error_code, None);
    assert_eq!(entry.resume_offset, Some(5242880));
}

#[test]
fn test_deserialize_user_options() {
    // User-defined options should be stored in options map
    let input = r#"http://example.com/file.zip
 GID=1
 split=4
 dir=/downloads
 TOTAL_LENGTH=1000
"#;

    let entry = SessionEntry::deserialize_line(input).unwrap();

    // Known fields parsed normally
    assert_eq!(entry.total_length, 1000);

    // User options stored in options
    assert_eq!(entry.options.get("split").unwrap(), "4");
    assert_eq!(entry.options.get("dir").unwrap(), "/downloads");
}

#[test]
fn test_bitfield_roundtrip() {
    let mut entry = SessionEntry::new(1, vec!["http://example.com/torrent.torrent".to_string()]);

    // Set bitfield: [0xFF, 0xF0, 0x0F] - indicates some pieces completed
    entry.bitfield = Some(vec![0xFF, 0xF0, 0x0F]);
    entry.num_pieces = Some(24); // 3 bytes * 8 bits = 24 pieces
    entry.piece_length = Some(262144); // 256KB

    let text = entry.serialize();

    // Verify hex encoding
    assert!(
        text.contains("BITFIELD=fff00f"),
        "bitfield should be encoded as hex string"
    );

    // Deserialize verification
    let restored = SessionEntry::deserialize_line(&text).unwrap();
    assert_eq!(
        restored.bitfield,
        Some(vec![0xFF, 0xF0, 0x0F]),
        "bitfield should be restored correctly"
    );
    assert_eq!(restored.num_pieces, Some(24));
    assert_eq!(restored.piece_length, Some(262144));
}

#[test]
fn test_empty_bitfield_serialized_as_empty() {
    let entry = SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
    // bitfield defaults to None

    let text = entry.serialize();

    // None bitfield should produce empty value
    assert!(
        text.contains("BITFIELD=\n"),
        "None bitfield should be serialized as empty value"
    );

    // Deserialize verification
    let restored = SessionEntry::deserialize_line(&text).unwrap();
    assert_eq!(
        restored.bitfield, None,
        "Empty bitfield should restore to None"
    );
}

#[test]
fn test_default_session_entry_has_zero_progress() {
    let entry = SessionEntry::new(99, vec!["http://test.com/f".to_string()]);

    // Verify all new fields have correct defaults
    assert_eq!(entry.total_length, 0);
    assert_eq!(entry.completed_length, 0);
    assert_eq!(entry.upload_length, 0);
    assert_eq!(entry.download_speed, 0);
    assert_eq!(entry.status, "active", "Default status should be 'active'");
    assert_eq!(entry.error_code, None);
    assert_eq!(entry.bitfield, None);
    assert_eq!(entry.num_pieces, None);
    assert_eq!(entry.piece_length, None);
    assert_eq!(entry.info_hash_hex, None);
    assert_eq!(entry.resume_offset, None);
}

#[test]
fn test_status_field_values() {
    let statuses = ["active", "waiting", "paused", "error"];

    for status in statuses {
        let mut entry = SessionEntry::new(1, vec!["http://example.com/f".to_string()]);
        entry.status = status.to_string();

        let text = entry.serialize();
        assert!(
            text.contains(&format!("STATUS={}", status)),
            "Status '{}' should be serialized correctly",
            status
        );

        // Deserialize verification
        let restored = SessionEntry::deserialize_line(&text).unwrap();
        assert_eq!(
            restored.status, status,
            "Status '{}' should be deserialized correctly",
            status
        );
    }
}

#[test]
fn test_resume_offset_for_http_ftp() {
    let mut entry = SessionEntry::new(1, vec!["http://example.com/large-file.iso".to_string()]);

    // Simulate HTTP download with partial data written
    entry.total_length = 1073741824; // 1GB
    entry.completed_length = 536870912; // 512MB completed
    entry.resume_offset = Some(536870912); // Resume from 512MB
    entry.status = "paused".to_string();

    let text = entry.serialize();

    // Verify resume offset is serialized correctly
    assert!(
        text.contains("RESUME_OFFSET=536870912"),
        "resume offset should be serialized correctly"
    );

    // Deserialize and verify
    let restored = SessionEntry::deserialize_line(&text).unwrap();
    assert_eq!(
        restored.resume_offset,
        Some(536870912),
        "resume offset should be restored correctly"
    );
    assert_eq!(restored.status, "paused");
}

#[test]
fn test_bt_specific_fields_only_when_present() {
    // Test that BT-specific fields are truly optional
    let mut entry = SessionEntry::new(1, vec!["magnet:?xt=urn:btih:abc123".to_string()]);

    // Don't set any BT fields (keep them as None)
    let text_without_bt = entry.serialize();
    let restored_without_bt = SessionEntry::deserialize_line(&text_without_bt).unwrap();

    assert_eq!(restored_without_bt.bitfield, None);
    assert_eq!(restored_without_bt.num_pieces, None);
    assert_eq!(restored_without_bt.piece_length, None);
    assert_eq!(restored_without_bt.info_hash_hex, None);

    // Now set BT fields
    entry.bitfield = Some(vec![0xAA, 0xBB]);
    entry.num_pieces = Some(16);
    entry.piece_length = Some(524288);
    entry.info_hash_hex = Some("abc123def456".to_string());

    let text_with_bt = entry.serialize();
    let restored_with_bt = SessionEntry::deserialize_line(&text_with_bt).unwrap();

    assert_eq!(restored_with_bt.bitfield, Some(vec![0xAA, 0xBB]));
    assert_eq!(restored_with_bt.num_pieces, Some(16));
    assert_eq!(restored_with_bt.piece_length, Some(524288));
    assert_eq!(
        restored_with_bt.info_hash_hex,
        Some("abc123def456".to_string())
    );
}

#[test]
fn test_download_options_to_map_all_fields() {
    // Verify that all non-default fields are serialized to the map
    let opts = DownloadOptions {
        split: Some(8),
        max_connection_per_server: Some(4),
        max_download_limit: Some(102400),
        max_upload_limit: Some(51200),
        dir: Some("/downloads".to_string()),
        out: Some("file.bin".to_string()),
        seed_time: Some(3600.0),
        seed_ratio: Some(2.0),
        // File allocation
        file_allocation: Some("trunc".to_string()),
        continue_download: true,
        allow_overwrite: true,
        auto_file_renaming: false,
        always_resume: false,
        max_resume_failure_tries: 2,
        remove_control_file: true,
        mmap_threshold: Some(128 * 1024 * 1024),
        secure_falloc: true,
        check_integrity: false,
        hash_check_only: false,
        bt_enable_hook_after_hash_check: true,
        bt_hash_check_seed: true,
        bt_seed_unverified: true,
        bt_tracker: Some(vec![
            "https://tracker.example/announce".to_string(),
            "udp://tracker.example:6969".to_string(),
        ]),
        // Checksum
        checksum: Some(("sha256".to_string(), "abc123".to_string())),
        // Cookies
        cookie_file: Some("/tmp/cookies.txt".to_string()),
        cookies: Some("key=value".to_string()),
        // BT
        bt_max_peers: 64,
        bt_force_encrypt: true,
        bt_require_crypto: true,
        enable_dht: false,
        dht_listen_port: Some("6881-6999".to_string()),
        index_out: None,
        dht_entry_point: Some(vec!["router.bittorrent.com:6881".to_string()]),
        enable_public_trackers: false,
        bt_piece_selection_strategy: "sequential".to_string(),
        bt_endgame_threshold: 10,
        bt_max_upload_slots: Some(4),
        bt_optimistic_unchoke_interval: Some(30),
        bt_snubbed_timeout: Some(60),
        bt_prioritize_piece: "head=512K".to_string(),
        bt_detach_seed_only: true,
        enable_utp: true,
        utp_listen_port: Some(6882),
        // Retry
        max_retries: 5,
        retry_wait: 3,
        // DHT file
        dht_file_path: Some("/tmp/dht.dat".to_string()),
        // Proxy
        http_proxy: Some("http://proxy:8080".to_string()),
        http_proxy_user: Some("http-user".to_string()),
        http_proxy_passwd: Some("http-pass".to_string()),
        all_proxy: Some("socks5://proxy:1080".to_string()),
        all_proxy_user: Some("all-user".to_string()),
        all_proxy_passwd: Some("all-pass".to_string()),
        https_proxy: Some("http://proxy:8443".to_string()),
        https_proxy_user: Some("https-user".to_string()),
        https_proxy_passwd: Some("https-pass".to_string()),
        ftp_proxy: Some("http://proxy:8021".to_string()),
        ftp_proxy_user: Some("ftp-user".to_string()),
        ftp_proxy_passwd: Some("ftp-pass".to_string()),
        no_proxy: Some("localhost,127.0.0.1".to_string()),
        // HTTP headers
        header: vec!["X-Custom: foo".to_string(), "X-Other: bar".to_string()],
        user_agent: Some("test-client/1.0".to_string()),
        referer: Some("http://example.com".to_string()),
        enable_http_keep_alive: false,
        enable_http_pipelining: true,
        http_accept_gzip: true,
        http_no_cache: true,
        use_head: true,
        no_want_digest_header: true,
        check_certificate: false,
        ca_certificate: Some("/tmp/ca.pem".to_string()),
        certificate: Some("/tmp/client.pem".to_string()),
        private_key: Some("/tmp/client.key".to_string()),
        min_tls_version: Some("TLSv1.3".to_string()),
        // Metalink
        metalink_version: None,
        metalink_language: None,
        metalink_os: None,
        metalink_location: None,
        metalink_preferred_protocol: None,
        select_file: None,
        bt_remove_unselected_file: true,
        piece_length: Some(1024 * 1024),
        metalink_enable_unique_protocol: false,
        // FTP
        timeout: Some(90),
        connect_timeout: Some(30),
        startup_idle_time: Some(10),
        lowest_speed_limit: Some(1024),
        ftp_pasv: false,
        remote_time: true,
        dry_run: true,
        ftp_reuse_connection: false,
        // Download
        realtime_chunk_checksum: false,
        bt_stop_timeout: Some(120),
        // BitTorrent extended
        disable_ipv6: true,
        listen_port: Some("6881-6999".to_string()),
        bt_enable_lpd: true,
        bt_lpd_interface: Some("eth0".to_string()),
        enable_rpc: false,
        pause: false,
        // Follow options
        follow_torrent: Some(FollowMode::Memory),
        follow_metalink: Some(FollowMode::Disabled),
        // Event hooks
        on_download_start: None,
        on_download_complete: None,
        on_download_error: None,
        on_download_pause: None,
        on_download_stop: None,
        on_bt_download_complete: None,
        // HTTP authentication
        http_auth_challenge: true,
        http_user: Some("http-user".to_string()),
        http_passwd: Some("http-pass".to_string()),
        ftp_user: Some("ftp-user".to_string()),
        ftp_passwd: Some("ftp-pass".to_string()),
        ssh_host_key_md: None,
        no_netrc: true,
        netrc_path: Some("/tmp/netrc".to_string()),
        // Conditional GET
        conditional_get: true,
    };

    let map = download_options_to_map(&opts);

    // File allocation
    assert_eq!(map.get("file-allocation").unwrap(), "trunc");
    assert_eq!(map.get("mmap-threshold").unwrap(), "134217728");
    assert_eq!(map.get("secure-falloc").unwrap(), "true");
    assert_eq!(map.get("continue").unwrap(), "true");
    assert_eq!(map.get("allow-overwrite").unwrap(), "true");
    assert_eq!(map.get("auto-file-renaming").unwrap(), "false");
    assert_eq!(map.get("always-resume").unwrap(), "false");
    assert_eq!(map.get("max-resume-failure-tries").unwrap(), "2");
    assert_eq!(map.get("remove-control-file").unwrap(), "true");

    // Checksum
    assert_eq!(map.get("checksum").unwrap(), "sha256=abc123");

    // Cookies
    assert_eq!(map.get("load-cookies").unwrap(), "/tmp/cookies.txt");
    assert_eq!(map.get("cookies").unwrap(), "key=value");

    // BT
    assert_eq!(map.get("bt-force-encrypt").unwrap(), "true");
    assert_eq!(map.get("bt-require-crypto").unwrap(), "true");
    assert_eq!(map.get("bt-seed-unverified").unwrap(), "true");
    assert_eq!(map.get("bt-max-peers").unwrap(), "64");
    assert_eq!(map.get("enable-dht").unwrap(), "false");
    assert_eq!(map.get("dht-listen-port").unwrap(), "6881-6999");
    assert_eq!(map.get("listen-port").unwrap(), "6881-6999");
    assert_eq!(
        map.get("dht-entry-point").unwrap(),
        "router.bittorrent.com:6881"
    );
    assert_eq!(
        map.get("bt-tracker").unwrap(),
        "https://tracker.example/announce,udp://tracker.example:6969"
    );
    assert_eq!(map.get("enable-public-trackers").unwrap(), "false");
    assert_eq!(
        map.get("bt-piece-selection-strategy").unwrap(),
        "sequential"
    );
    assert_eq!(map.get("bt-endgame-threshold").unwrap(), "10");
    assert_eq!(map.get("bt-max-upload-slots").unwrap(), "4");
    assert_eq!(map.get("bt-optimistic-unchoke-interval").unwrap(), "30");
    assert_eq!(map.get("bt-snubbed-timeout").unwrap(), "60");
    assert_eq!(map.get("bt-prioritize-piece").unwrap(), "head=512K");
    assert_eq!(map.get("bt-remove-unselected-file").unwrap(), "true");
    assert_eq!(map.get("enable-utp").unwrap(), "true");
    assert_eq!(map.get("utp-listen-port").unwrap(), "6882");
    assert_eq!(map.get("follow-torrent").unwrap(), "mem");
    assert_eq!(map.get("follow-metalink").unwrap(), "false");

    // Connection and authentication options
    assert_eq!(map.get("piece-length").unwrap(), "1048576");
    assert_eq!(map.get("metalink-enable-unique-protocol").unwrap(), "false");
    assert_eq!(map.get("timeout").unwrap(), "90");
    assert_eq!(map.get("connect-timeout").unwrap(), "30");
    assert_eq!(map.get("startup-idle-time").unwrap(), "10");
    assert_eq!(map.get("lowest-speed-limit").unwrap(), "1024");
    assert_eq!(map.get("ftp-pasv").unwrap(), "false");
    assert_eq!(map.get("remote-time").unwrap(), "true");
    assert_eq!(map.get("dry-run").unwrap(), "true");
    assert_eq!(map.get("ftp-reuse-connection").unwrap(), "false");
    assert_eq!(map.get("realtime-chunk-checksum").unwrap(), "false");
    assert_eq!(map.get("bt-stop-timeout").unwrap(), "120");
    assert_eq!(map.get("disable-ipv6").unwrap(), "true");
    assert_eq!(map.get("bt-enable-lpd").unwrap(), "true");
    assert_eq!(map.get("bt-lpd-interface").unwrap(), "eth0");
    assert_eq!(map.get("http-auth-challenge").unwrap(), "true");
    assert_eq!(map.get("http-user").unwrap(), "http-user");
    assert_eq!(map.get("http-passwd").unwrap(), "http-pass");
    assert_eq!(map.get("ftp-user").unwrap(), "ftp-user");
    assert_eq!(map.get("ftp-passwd").unwrap(), "ftp-pass");
    assert_eq!(map.get("no-netrc").unwrap(), "true");
    assert_eq!(map.get("netrc-path").unwrap(), "/tmp/netrc");
    assert_eq!(map.get("conditional-get").unwrap(), "true");

    // Retry
    assert_eq!(map.get("max-retries").unwrap(), "5");
    assert_eq!(map.get("retry-wait").unwrap(), "3");

    // DHT file
    assert_eq!(map.get("dht-file-path").unwrap(), "/tmp/dht.dat");

    // Proxy
    assert_eq!(map.get("http-proxy").unwrap(), "http://proxy:8080");
    assert_eq!(map.get("all-proxy").unwrap(), "socks5://proxy:1080");
    assert_eq!(map.get("https-proxy").unwrap(), "http://proxy:8443");
    assert_eq!(map.get("ftp-proxy").unwrap(), "http://proxy:8021");
    assert_eq!(map.get("no-proxy").unwrap(), "localhost,127.0.0.1");

    // HTTP headers
    assert_eq!(map.get("header").unwrap(), "X-Custom: foo,X-Other: bar");
    assert_eq!(map.get("user-agent").unwrap(), "test-client/1.0");
    assert_eq!(map.get("referer").unwrap(), "http://example.com");
    assert_eq!(map.get("enable-http-keep-alive").unwrap(), "false");
    assert_eq!(map.get("enable-http-pipelining").unwrap(), "true");
    assert_eq!(map.get("http-accept-gzip").unwrap(), "true");
    assert_eq!(map.get("http-no-cache").unwrap(), "true");
    assert_eq!(map.get("use-head").unwrap(), "true");
    assert_eq!(map.get("no-want-digest-header").unwrap(), "true");
    assert_eq!(map.get("check-certificate").unwrap(), "false");
    assert_eq!(map.get("ca-certificate").unwrap(), "/tmp/ca.pem");
    assert_eq!(map.get("certificate").unwrap(), "/tmp/client.pem");
    assert_eq!(map.get("private-key").unwrap(), "/tmp/client.key");
    assert_eq!(map.get("min-tls-version").unwrap(), "TLSv1.3");

    // The same canonical string map is consumed by session restoration.
    let restored = DownloadOptions::from_option_strings(&map);
    assert!(restored.continue_download);
    assert!(!restored.auto_file_renaming);
    assert!(!restored.always_resume);
    assert_eq!(restored.max_resume_failure_tries, 2);
    assert_eq!(restored.cookie_file.as_deref(), Some("/tmp/cookies.txt"));
    assert_eq!(restored.bt_max_peers, 64);
    assert_eq!(restored.certificate.as_deref(), Some("/tmp/client.pem"));
    assert_eq!(restored.private_key.as_deref(), Some("/tmp/client.key"));
    assert_eq!(
        restored.bt_tracker,
        Some(vec![
            "https://tracker.example/announce".to_string(),
            "udp://tracker.example:6969".to_string(),
        ])
    );
    assert_eq!(restored.listen_port.as_deref(), Some("6881-6999"));
    assert_eq!(restored.dht_listen_port.as_deref(), Some("6881-6999"));
    assert!(!restored.metalink_enable_unique_protocol);
    assert_eq!(restored.piece_length, Some(1024 * 1024));
    assert_eq!(restored.follow_torrent, Some(FollowMode::Memory));
    assert_eq!(restored.follow_metalink, Some(FollowMode::Disabled));
    assert!(restored.bt_seed_unverified);
    assert!(!restored.ftp_pasv);
    assert_eq!(restored.http_user.as_deref(), Some("http-user"));
    assert!(restored.conditional_get);
}

#[test]
fn test_download_options_to_map_defaults_excluded() {
    // Default DownloadOptions should produce a minimal map.
    // enable_dht and enable_public_trackers default to true (matching the
    // load path's `unwrap_or(true)`), so they are NOT saved when at the
    // default value -- the save logic only serializes them when disabled.
    let opts = DownloadOptions::default();
    let map = download_options_to_map(&opts);

    // secure_falloc defaults to false -> should NOT be in map
    assert!(!map.contains_key("secure-falloc"));
    // file_allocation defaults to None -> should NOT be in map
    assert!(!map.contains_key("file-allocation"));
    assert!(!map.contains_key("seed-ratio"));
    assert!(!map.contains_key("metalink-enable-unique-protocol"));
    assert!(!map.contains_key("load-cookies"));
    // enable_dht and enable_public_trackers default to true -> NOT saved
    assert!(!map.contains_key("enable-dht"));
    assert!(!map.contains_key("enable-public-trackers"));

    // The omitted wire value must still restore aria2's typed default.
    let restored = DownloadOptions::from_option_strings(&map);
    assert_eq!(restored.seed_ratio, Some(1.0));
}
