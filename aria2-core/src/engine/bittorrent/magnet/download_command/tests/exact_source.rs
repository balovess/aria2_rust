use super::*;

#[tokio::test]
async fn magnet_exact_source_returns_matching_torrent_without_dht() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let torrent = build_test_torrent();
    let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent should parse")
        .info_hash
        .bytes;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind exact-source test server");
    let address = listener.local_addr().expect("read test server address");
    let server_torrent = torrent.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .expect("accept exact-source request");
        let mut request = [0u8; 4096];
        let _request_len = socket
            .read(&mut request)
            .await
            .expect("read exact-source request");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            server_torrent.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write exact-source headers");
        socket
            .write_all(&server_torrent)
            .await
            .expect("write exact-source torrent");
    });

    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
        "magnet:?xt=urn:btih:{}&xs=http://{}/metadata.torrent",
        hex::encode(info_hash),
        address
    ))
    .expect("exact-source magnet should parse");
    assert_eq!(magnet.exact_sources.len(), 1);
    MagnetDownloadCommand::metadata_matches_magnet(&magnet, &torrent)
        .expect("test magnet hash should match test torrent");
    let command = make_test_command();
    let metadata = command
        .fetch_magnet_exact_source(&magnet, &DownloadOptions::default())
        .await
        .expect("exact source should return metadata");
    server.await.expect("exact-source server should finish");

    assert_eq!(metadata, torrent);
    assert!(command.dht_engines.is_empty());
}

#[tokio::test]
async fn magnet_exact_source_reads_file_url_without_dht() {
    let torrent = build_test_torrent();
    let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent should parse")
        .info_hash
        .bytes;
    let temp_dir = tempfile::tempdir().expect("temporary exact-source directory");
    let path = temp_dir.path().join("metadata.torrent");
    std::fs::write(&path, &torrent).expect("write exact-source torrent");
    let source =
        url::Url::from_file_path(&path).expect("temporary path should convert to a file URL");
    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
        "magnet:?xt=urn:btih:{}&xs={source}",
        hex::encode(info_hash)
    ))
    .expect("file exact-source magnet should parse");

    let command = make_test_command();
    let metadata = command
        .fetch_magnet_exact_source(&magnet, &DownloadOptions::default())
        .await
        .expect("file exact source should return metadata");

    assert_eq!(metadata, torrent);
    assert!(command.dht_engines.is_empty());
}

#[tokio::test]
async fn magnet_exact_source_retries_http_basic_auth_challenge() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let torrent = build_test_torrent();
    let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent should parse")
        .info_hash
        .bytes;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth exact-source test server");
    let address = listener.local_addr().expect("read auth server address");
    let server_torrent = torrent.clone();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept auth request");
            let mut request = [0u8; 4096];
            let length = socket.read(&mut request).await.expect("read auth request");
            let request = String::from_utf8_lossy(&request[..length]);
            if attempt == 0 {
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                socket
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=exact\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth challenge");
            } else {
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: basic dxnlcjpwyxnz")
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    server_torrent.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&server_torrent).await.unwrap();
            }
        }
    });

    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
        "magnet:?xt=urn:btih:{}&xs=http://{address}/metadata.torrent",
        hex::encode(info_hash)
    ))
    .expect("auth exact-source magnet should parse");
    let options = DownloadOptions {
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..DownloadOptions::default()
    };
    let metadata = make_test_command()
        .fetch_magnet_exact_source(&magnet, &options)
        .await
        .expect("authenticated exact source should return metadata");
    server
        .await
        .expect("auth exact-source server should finish");
    assert_eq!(metadata, torrent);
}

#[tokio::test]
async fn magnet_exact_source_uses_authenticated_http_proxy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let torrent = build_test_torrent();
    let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent should parse")
        .info_hash
        .bytes;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy exact-source test server");
    let address = listener.local_addr().expect("read proxy server address");
    let server_torrent = torrent.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept proxy request");
        let mut request = [0u8; 8192];
        let length = socket.read(&mut request).await.expect("read proxy request");
        let request = String::from_utf8_lossy(&request[..length]);
        assert!(request.starts_with("GET http://exact-source.invalid/metadata.torrent"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic dxnlcjpwyxnz")
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            server_torrent.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(&server_torrent).await.unwrap();
    });

    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
        "magnet:?xt=urn:btih:{}&xs=http://exact-source.invalid/metadata.torrent",
        hex::encode(info_hash)
    ))
    .expect("proxy exact-source magnet should parse");
    let options = DownloadOptions {
        http_proxy: Some(format!("http://{address}")),
        http_proxy_user: Some("user".to_string()),
        http_proxy_passwd: Some("pass".to_string()),
        no_proxy: Some(String::new()),
        ..DownloadOptions::default()
    };
    let metadata = make_test_command()
        .fetch_magnet_exact_source(&magnet, &options)
        .await
        .expect("proxied exact source should return metadata");
    server
        .await
        .expect("proxy exact-source server should finish");
    assert_eq!(metadata, torrent);
}
