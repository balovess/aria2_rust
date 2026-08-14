use super::*;
#[cfg(feature = "bittorrent")]
use crate::engine::command::Command;
use crate::request::request_group::{DownloadOptions, GroupId};
use aria2_protocol::metalink::parser::UrlEntry;

#[test]
fn test_priority_ascending_order() {
    let urls = vec![
        UrlEntry::new("http://mirror3.example.com/file.bin").with_priority(3),
        UrlEntry::new("http://mirror1.example.com/file.bin").with_priority(1),
        UrlEntry::new("http://mirror2.example.com/file.bin").with_priority(2),
    ];

    let sorted = select_mirrors_by_priority(&urls, "");

    assert_eq!(sorted.len(), 3);
    assert_eq!(sorted[0].priority, 1);
    assert_eq!(sorted[1].priority, 2);
    assert_eq!(sorted[2].priority, 3);
}

#[test]
fn test_location_preference_boosts_matching() {
    let urls = vec![
        UrlEntry::new("http://us-mirror1.example.com/file.bin")
            .with_priority(5)
            .with_location("us"),
        UrlEntry::new("http://eu-mirror1.example.com/file.bin")
            .with_priority(5)
            .with_location("eu"),
        UrlEntry::new("http://eu-mirror2.example.com/file.bin")
            .with_priority(5)
            .with_location("eu"),
        UrlEntry::new("http://jp-mirror1.example.com/file.bin")
            .with_priority(5)
            .with_location("jp"),
    ];

    let sorted = select_mirrors_by_priority(&urls, "eu");

    assert_eq!(sorted.len(), 4);

    // EU mirrors should appear before non-EU mirrors
    let first_non_eu_idx = sorted
        .iter()
        .position(|u| u.location.as_deref() != Some("eu"))
        .expect("Should find at least one non-EU mirror");

    let last_eu_idx = sorted
        .iter()
        .rposition(|u| u.location.as_deref() == Some("eu"))
        .expect("Should find EU mirrors");

    assert!(last_eu_idx < first_non_eu_idx);
}

#[tokio::test]
async fn test_failover_tries_all_then_errors() {
    let urls = [
        UrlEntry::new("http://mirror1.fail/file.bin").with_priority(3),
        UrlEntry::new("http://mirror2.fail/file.bin").with_priority(2),
        UrlEntry::new("http://mirror3.fail/file.bin").with_priority(1),
    ];

    let fail_fn = |url: &str| -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<Vec<u8>, String>> + '_>,
    > {
        let url_owned = url.to_string();
        Box::pin(async move { Err(format!("Connection refused to {}", url_owned)) })
    };

    let url_refs: Vec<&UrlEntry> = urls.iter().collect();
    let result = try_mirrors_with_failover(&url_refs, fail_fn).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("All 3 mirrors failed"));
}

#[tokio::test]
async fn test_single_mirror_no_failover_needed() {
    let urls = [UrlEntry::new("http://working-mirror.example.com/success.bin").with_priority(10)];

    let expected_data = b"Downloaded file content".to_vec();
    let data_shared = std::sync::Arc::new(expected_data.clone());
    let success_fn = move |_url: &str| {
        let data = data_shared.clone();
        async move { Ok((*data).clone()) }
    };

    let result = try_mirrors_with_failover(&urls.iter().collect::<Vec<_>>(), &success_fn).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), expected_data);
}

#[test]
fn test_priority_overrides_location() {
    let urls = vec![
        UrlEntry::new("http://high-eu.example.com/file.bin")
            .with_priority(10)
            .with_location("eu"),
        UrlEntry::new("http://low-us.example.com/file.bin")
            .with_priority(1)
            .with_location("us"),
    ];

    let sorted = select_mirrors_by_priority(&urls, "eu");
    assert_eq!(sorted[0].priority, 1);
    assert_eq!(sorted[1].priority, 10);
}

#[test]
fn test_empty_resources_returns_empty() {
    let urls: Vec<UrlEntry> = Vec::new();
    let sorted = select_mirrors_by_priority(&urls, "");
    assert!(sorted.is_empty());
}

#[tokio::test]
async fn test_failover_succeeds_on_second_mirror() {
    let urls = [
        UrlEntry::new("http://failing-mirror.example.com/file.bin").with_priority(1),
        UrlEntry::new("http://working-mirror.example.com/file.bin").with_priority(2),
    ];

    let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_clone = attempt_count.clone();
    let fallback_fn = move |url: &str| {
        let url_owned = url.to_string();
        let count = count_clone.clone();
        async move {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if url_owned.contains("failing") {
                Err("Connection timeout".to_string())
            } else {
                Ok(b"Success data".to_vec())
            }
        }
    };

    let result = try_mirrors_with_failover(&urls.iter().collect::<Vec<_>>(), &fallback_fn).await;

    assert!(result.is_ok());
    assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(result.unwrap(), b"Success data");
}

// ── Multi-file Metalink tests ─────────────────────────────────────────

fn make_multi_file_xml() -> Vec<u8> {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="first.bin">
      <size>1024</size>
      <hash type="sha-256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</hash>
      <url priority="1">http://mirror.example.com/first.bin</url>
    </file>
    <file name="second.bin">
      <size>2048</size>
      <hash type="sha-256">bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</hash>
      <url priority="1">http://mirror.example.com/second.bin</url>
    </file>
  </files>
</metalink>"#
        .as_bytes()
        .to_vec()
}

#[test]
fn test_new_accepts_multi_file_metalink() {
    let options = DownloadOptions::default();
    // Previously this would return "Metalink contains multiple files or no files"
    // Now it should succeed, picking the first file
    let result =
        MetalinkDownloadCommand::new(GroupId::new(1), &make_multi_file_xml(), &options, None);
    assert!(result.is_ok(), "new() should accept multi-file Metalink");
}

#[test]
fn test_single_file_mode_accepts_torrent_metaurl_only() {
    use aria2_protocol::metalink::parser::{MediaType, MetalinkDocument};

    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="movie.mkv">
    <size>1048576</size>
    <metaurl mediatype="application/x-bittorrent" priority="1">http://mirror.example.com/movie.torrent</metaurl>
  </file>
</metalink>"#;
    let options = DownloadOptions::default();
    // A torrent metaurl is a valid download path (C++ BtDependency): the
    // constructor must NOT reject the file just because it has no HTTP URL.
    let cmd = MetalinkDownloadCommand::new(GroupId::new(1), xml.as_bytes(), &options, None)
        .expect("metaurl-only single file accepted");

    // execute() re-parses metalink_data; verify the raw data round-trips and
    // carries the torrent metaurl so the BT dependency path can be taken.
    assert!(!cmd.metalink_data.is_empty());
    let doc = MetalinkDocument::parse(&cmd.metalink_data, None).unwrap();
    let f = &doc.files[0];
    assert_eq!(f.meta_urls.len(), 1);
    assert_eq!(
        f.meta_urls[0].url,
        "http://mirror.example.com/movie.torrent"
    );
    assert_eq!(f.meta_urls[0].mediatype, MediaType::Torrent);
    assert!(f.urls.is_empty(), "no HTTP mirrors in this file");
}

/// Compute a lowercase hex sha-256 digest (test helper).
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[test]
fn test_verify_pieces() {
    use aria2_protocol::metalink::parser::{HashAlgorithm, PieceInfo};

    let options = DownloadOptions::default();
    let cmd = MetalinkDownloadCommand::new(GroupId::new(1), &make_multi_file_xml(), &options, None)
        .expect("command constructs");

    // 8 bytes split into 2 chunks of 4 bytes.
    let data = b"aaaabbbb".to_vec();
    let h0 = sha256_hex(&data[0..4]);
    let h1 = sha256_hex(&data[4..8]);
    let pieces = PieceInfo {
        length: 4,
        type_: HashAlgorithm::Sha256,
        hashes: vec![h0.clone(), h1.clone()],
    };

    // All chunks match.
    assert!(cmd.verify_pieces(&data, &pieces).unwrap());

    // Tampered byte in chunk 0 → mismatch.
    let mut bad = data.clone();
    bad[0] ^= 0xFF;
    assert!(!cmd.verify_pieces(&bad, &pieces).unwrap());

    // Tampered byte in the last chunk → mismatch.
    let mut bad_last = data.clone();
    bad_last[7] ^= 0x01;
    assert!(!cmd.verify_pieces(&bad_last, &pieces).unwrap());

    // Digest count mismatch → fail loudly.
    let short = PieceInfo {
        hashes: vec![h0],
        ..pieces.clone()
    };
    assert!(!cmd.verify_pieces(&data, &short).unwrap());

    // Wrong digest length → fail.
    let wrong_len = PieceInfo {
        hashes: vec!["deadbeef".to_string(), h1],
        ..pieces.clone()
    };
    assert!(!cmd.verify_pieces(&data, &wrong_len).unwrap());

    // Empty hash list → trivially passes (no chunk verification requested).
    let empty = PieceInfo {
        hashes: Vec::new(),
        ..pieces
    };
    assert!(cmd.verify_pieces(&data, &empty).unwrap());
}

#[test]
fn test_create_multi_file_returns_all_files() {
    let options = DownloadOptions::default();
    let commands =
        MetalinkDownloadCommand::create_multi_file(&make_multi_file_xml(), &options, None, 100)
            .unwrap();

    assert_eq!(commands.len(), 2, "Should create 2 commands for 2 files");
    assert_eq!(commands[0].file_index, 0);
    assert_eq!(commands[1].file_index, 1);
    assert!(
        commands[0]
            .command
            .output_path
            .to_string_lossy()
            .contains("first.bin"),
        "First command should be for first.bin"
    );
    assert!(
        commands[1]
            .command
            .output_path
            .to_string_lossy()
            .contains("second.bin"),
        "Second command should be for second.bin"
    );
}

#[test]
fn test_create_multi_file_assigns_incrementing_gids() {
    let options = DownloadOptions::default();
    let commands =
        MetalinkDownloadCommand::create_multi_file(&make_multi_file_xml(), &options, None, 200)
            .unwrap();

    let g0 = commands[0].command.group.read().unwrap();
    let g1 = commands[1].command.group.read().unwrap();
    assert_eq!(g0.gid().value(), 200);
    assert_eq!(g1.gid().value(), 201);
}

#[test]
fn test_create_multi_file_keeps_torrent_metaurl_only_files() {
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="payload.bin">
    <metaurl mediatype="application/x-bittorrent">http://mirror.example.com/payload.torrent</metaurl>
  </file>
</metalink>"#;
    let result =
        MetalinkDownloadCommand::create_multi_file(xml, &DownloadOptions::default(), None, 1)
            .unwrap();

    assert_eq!(result.len(), 1);
    assert!(result[0].command.group().uris().is_empty());
}

#[test]
fn test_create_multi_file_skips_files_without_urls() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="with-urls.bin">
      <size>1024</size>
      <url priority="1">http://mirror.example.com/with-urls.bin</url>
    </file>
    <file name="no-urls.bin">
      <size>2048</size>
    </file>
  </files>
</metalink>"#;

    let options = DownloadOptions::default();
    let commands =
        MetalinkDownloadCommand::create_multi_file(xml.as_bytes(), &options, None, 1).unwrap();

    assert_eq!(commands.len(), 1, "Should skip file with no URLs");
    assert_eq!(commands[0].file_index, 0);
}

#[test]
fn test_output_path_accessor() {
    let options = DownloadOptions::default();
    let commands = MetalinkDownloadCommand::create_multi_file(
        &make_multi_file_xml(),
        &options,
        Some("/tmp"),
        1,
    )
    .unwrap();

    assert!(
        commands[0]
            .command
            .output_path()
            .to_string_lossy()
            .contains("first.bin")
    );
}

#[test]
fn test_new_rejects_invalid_client_identity_configuration() {
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="payload.bin">
      <size>1</size>
      <url>http://mirror.example.com/payload.bin</url>
    </file>
  </files>
</metalink>"#;
    let options = DownloadOptions {
        certificate: Some("missing-client.pem".to_string()),
        private_key: Some("missing-client.key".to_string()),
        ..DownloadOptions::default()
    };

    let error = match MetalinkDownloadCommand::new(GroupId::new(90), xml, &options, None) {
        Ok(_) => panic!("invalid client identity must reject Metalink client construction"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("Failed to read client certificate")
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn shared_metaurl_fallback_downloads_one_torrent_for_all_files() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use aria2_protocol::metalink::parser::MetalinkDocument;
    use std::collections::BTreeMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let torrent_bytes = {
        let mut torrent_files = Vec::new();
        for path in [["dir1", "file1.txt"], ["dir2", "file2.dat"]] {
            let mut file = BTreeMap::new();
            file.insert(b"length".to_vec(), BencodeValue::Int(0));
            file.insert(
                b"path".to_vec(),
                BencodeValue::List(
                    path.iter()
                        .map(|part| BencodeValue::Bytes(part.as_bytes().to_vec()))
                        .collect(),
                ),
            );
            torrent_files.push(BencodeValue::Dict(file));
        }
        let mut info = BTreeMap::new();
        info.insert(b"files".to_vec(), BencodeValue::List(torrent_files));
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"bundle".to_vec()));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(16_384));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(Vec::new()));
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.test/announce".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));
        BencodeValue::Dict(root).encode()
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fallback server should bind");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let request_count = Arc::new(AtomicUsize::new(0));
    let torrent_count = Arc::new(AtomicUsize::new(0));
    let server_request_count = Arc::clone(&request_count);
    let server_torrent_count = Arc::clone(&torrent_count);
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("fallback request");
            let mut request = [0u8; 2048];
            let length = stream.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..length]);
            let is_torrent = request.contains("GET /shared.torrent ");
            server_request_count.fetch_add(1, Ordering::SeqCst);
            if is_torrent {
                server_torrent_count.fetch_add(1, Ordering::SeqCst);
            }
            let (status, body) = if is_torrent {
                ("200 OK", torrent_bytes.clone())
            } else {
                ("500 Internal Server Error", Vec::new())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response header");
            stream.write_all(&body).await.expect("write response body");
            if server_request_count.load(Ordering::SeqCst) >= 3 {
                break;
            }
        }
    });

    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="first.bin">
      <size>0</size>
      <url>{base_url}/first.bin</url>
      <metaurl mediatype="application/x-bittorrent" name="dir1/file1.txt">{base_url}/shared.torrent</metaurl>
    </file>
    <file name="second.bin">
      <size>0</size>
      <url>{base_url}/second.bin</url>
      <metaurl mediatype="application/x-bittorrent" name="dir2/file2.dat">{base_url}/shared.torrent</metaurl>
    </file>
  </files>
</metalink>"#
    );
    let document = MetalinkDocument::parse(xml.as_bytes(), None).expect("Metalink parses");
    let options = DownloadOptions {
        dir: Some(output_dir.path().to_string_lossy().into_owned()),
        ..DownloadOptions::default()
    };
    let mut command = MetalinkDownloadCommand::create_multi_file_group(
        &document.files,
        &options,
        options.dir.as_deref(),
        105,
    )
    .expect("shared command constructs");

    command.execute().await.expect("shared fallback completes");
    server.await.expect("fallback server completes");

    assert_eq!(request_count.load(Ordering::SeqCst), 3);
    assert_eq!(torrent_count.load(Ordering::SeqCst), 1);
    let context = command
        .group()
        .get_download_context()
        .expect("torrent fallback context");
    let entries = context.get_file_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].original_name(), "dir1/file1.txt");
    assert_eq!(entries[1].original_name(), "dir2/file2.dat");
    assert_eq!(
        entries[0].path(),
        output_dir.path().join("first.bin").to_string_lossy()
    );
    assert_eq!(
        entries[1].path(),
        output_dir.path().join("second.bin").to_string_lossy()
    );
    assert!(entries.iter().all(|entry| entry.is_requested()));
}
