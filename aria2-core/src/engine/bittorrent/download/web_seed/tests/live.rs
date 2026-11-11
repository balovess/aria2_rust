use super::*;

#[tokio::test]
async fn live_web_seed_uses_per_file_ranges_and_observes_change_uri() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn range_server(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local web-seed fixture");
        let address = listener.local_addr().expect("get fixture address");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept web-seed request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 512];
            loop {
                let count = stream.read(&mut buffer).await.expect("read request");
                assert_ne!(count, 0, "request ended before headers completed");
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(request.contains("range: bytes=0-3"));
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("write response headers");
            stream.write_all(body).await.expect("write response body");
        });
        (format!("http://{address}/file"), task)
    }

    let (first_url, first_server) = range_server(b"ABCD").await;
    let (second_url, second_server) = range_server(b"EFGH").await;
    let entries = vec![
        crate::download::file_entry::FileEntry::new("first".into(), 4, 0, vec![first_url]),
        crate::download::file_entry::FileEntry::new("second".into(), 4, 4, Vec::new()),
    ];
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(8);
    context.set_file_entries(entries);
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7101),
            Vec::new(),
            Default::default(),
        ),
    ));
    group
        .recover()
        .set_download_context(std::sync::Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        std::sync::Arc::clone(&group),
        8,
        8,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    assert!(!manager.is_empty());
    assert!(!manager.has_complete_sources_for_piece(0, 8));
    let original_uri_generation = group.recover().uri_generation();
    group
        .recover_mut()
        .change_uris(2, &[], &[second_url], None)
        .expect("changeUri should add the second file source");
    assert!(group.recover().uri_generation() > original_uri_generation);
    assert!(manager.has_complete_sources_for_piece(0, 8));
    let data = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        manager.request_piece_with_length_and_activity(0, 8, None),
    )
    .await
    .expect("live WebSeed request should not hang")
    .expect("per-file sources should supply the piece");

    assert_eq!(data, b"ABCDEFGH");
    first_server.await.expect("first file source should finish");
    second_server
        .await
        .expect("second file source should finish");
}

#[tokio::test]
async fn live_web_seed_stops_reprobing_404_until_uri_configuration_changes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn fixture(
        response_body: &'static [u8],
        status: &'static str,
        request_count: usize,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local WebSeed fixture");
        let address = listener.local_addr().expect("get fixture address");
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_requests = std::sync::Arc::clone(&requests);
        let task = tokio::spawn(async move {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().await.expect("accept WebSeed request");
                let mut request = Vec::new();
                let mut buffer = [0u8; 512];
                loop {
                    let count = stream.read(&mut buffer).await.expect("read request");
                    assert_ne!(count, 0, "request ended before headers completed");
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                server_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let response_data = if status.starts_with("206") {
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let range = request
                        .lines()
                        .find_map(|line| line.strip_prefix("range: bytes="))
                        .expect("WebSeed request includes a byte range");
                    let (start, end) = range.split_once('-').expect("parse requested range");
                    let start = start.parse::<usize>().expect("range start is numeric");
                    let end = end.parse::<usize>().expect("range end is numeric");
                    &response_body[start..=end]
                } else {
                    &[]
                };
                let headers = if status.starts_with("206") {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response_data.len()
                    )
                } else {
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                };
                stream
                    .write_all(headers.as_bytes())
                    .await
                    .expect("write response headers");
                if !response_data.is_empty() {
                    stream
                        .write_all(response_data)
                        .await
                        .expect("write response body");
                }
            }
        });
        (format!("http://{address}/file"), requests, task)
    }

    let (dead_url, dead_requests, dead_server) = fixture(b"", "404 Not Found", 2).await;
    let (healthy_url, healthy_requests, healthy_server) =
        fixture(b"ABCDEFGHIJKL", "206 Partial Content", 3).await;
    let entry = crate::download::file_entry::FileEntry::new(
        "file.bin".into(),
        12,
        0,
        vec![dead_url, healthy_url],
    );
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(4);
    context.set_file_entries(vec![entry]);
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7103),
            Vec::new(),
            Default::default(),
        ),
    ));
    group
        .recover()
        .set_download_context(std::sync::Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        std::sync::Arc::clone(&group),
        4,
        12,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    for (piece_index, expected) in [(0, b"ABCD".as_slice()), (1, b"EFGH".as_slice())] {
        let data = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            manager.request_piece_with_length_and_activity(piece_index, 4, None),
        )
        .await
        .expect("WebSeed request should not hang")
        .expect("healthy mirror should serve each piece");
        assert_eq!(data, expected);
    }

    let uri_states = group.recover().uri_entries();
    assert_eq!(uri_states.len(), 2);
    assert!(
        uri_states.iter().all(|entry| entry.status == "used"),
        "both dispatched WebSeed sources should be projected as used: {uri_states:?}"
    );

    assert_eq!(
        dead_requests.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the 404 source should be skipped for later pieces in the same URI generation"
    );
    group
        .recover_mut()
        .change_uris(1, &[], &["http://127.0.0.1:9/unused.bin".to_string()], None)
        .expect("changeUri should advance the source configuration generation");
    let data = manager
        .request_piece_with_length_and_activity(2, 4, None)
        .await
        .expect("healthy mirror should still serve after URI configuration changes");
    assert_eq!(data, b"IJKL");
    let changed_uri_states = group.recover().uri_entries();
    assert_eq!(
        changed_uri_states
            .iter()
            .filter(|entry| entry.status == "used")
            .count(),
        2,
        "previously dispatched URIs should remain used after adding a source"
    );
    assert_eq!(
        changed_uri_states
            .iter()
            .filter(|entry| entry.status == "waiting")
            .count(),
        1,
        "the newly added URI should remain waiting until selected"
    );

    dead_server.await.expect("404 fixture should finish");
    healthy_server.await.expect("healthy fixture should finish");
    assert_eq!(
        dead_requests.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "a 404 source should be probed again only after URI configuration changes"
    );
    assert_eq!(
        healthy_requests.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "the healthy mirror should serve all piece ranges"
    );
}
