use super::WebSeedManager;
use crate::util::rwlock_ext::RwLockRecover;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Barrier, mpsc, oneshot};

async fn read_request_headers(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0u8; 512];
    loop {
        let count = stream
            .read(&mut buffer)
            .await
            .expect("read WebSeed request");
        assert_ne!(count, 0, "WebSeed request ended before headers completed");
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8_lossy(&request).to_ascii_lowercase();
        }
    }
}

fn requested_range(request: &str) -> (usize, usize) {
    let range = request
        .lines()
        .find_map(|line| line.strip_prefix("range: bytes="))
        .expect("WebSeed request includes a byte range");
    let (start, end) = range.split_once('-').expect("parse WebSeed byte range");
    (
        start.parse().expect("range start is numeric"),
        end.parse().expect("range end is numeric"),
    )
}

fn partial_response_headers(length: usize) -> String {
    format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
}

#[tokio::test]
async fn concurrent_live_web_seed_pieces_share_the_initial_404_probe() {
    let dead_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind dead WebSeed fixture");
    let dead_address = dead_listener.local_addr().expect("dead WebSeed address");
    let dead_requests = Arc::new(AtomicUsize::new(0));
    let server_dead_requests = Arc::clone(&dead_requests);
    let (release_dead_tx, mut release_dead_rx) = oneshot::channel();
    let (stop_dead_tx, mut stop_dead_rx) = oneshot::channel();
    let dead_server = tokio::spawn(async move {
        let mut released = false;
        let mut pending = Vec::new();
        loop {
            tokio::select! {
                accepted = dead_listener.accept() => {
                    let (mut stream, _) = accepted.expect("accept dead WebSeed request");
                    read_request_headers(&mut stream).await;
                    server_dead_requests.fetch_add(1, Ordering::Relaxed);
                    if released {
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await
                            .expect("write dead WebSeed response");
                    } else {
                        pending.push(stream);
                    }
                }
                _ = &mut release_dead_rx, if !released => {
                    released = true;
                    for mut stream in pending.drain(..) {
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await
                            .expect("release held 404 response");
                    }
                }
                _ = &mut stop_dead_rx => break,
            }
        }
    });

    let healthy_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind healthy WebSeed fixture");
    let healthy_address = healthy_listener
        .local_addr()
        .expect("healthy WebSeed address");
    let healthy_requests = Arc::new(AtomicUsize::new(0));
    let server_healthy_requests = Arc::clone(&healthy_requests);
    let healthy_server = tokio::spawn(async move {
        let mut pending = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = healthy_listener
                .accept()
                .await
                .expect("accept healthy WebSeed request");
            let request = read_request_headers(&mut stream).await;
            server_healthy_requests.fetch_add(1, Ordering::Relaxed);
            let (start, end) = requested_range(&request);
            let body = b"ABCDEFGH"[start..=end].to_vec();
            stream
                .write_all(partial_response_headers(body.len()).as_bytes())
                .await
                .expect("write healthy WebSeed headers");
            pending.push((stream, body));
        }
        for (mut stream, body) in pending {
            stream
                .write_all(&body)
                .await
                .expect("release healthy WebSeed body");
        }
    });

    let entry = crate::download::file_entry::FileEntry::new(
        "file.bin".into(),
        8,
        0,
        vec![
            format!("http://{dead_address}/file"),
            format!("http://{healthy_address}/file"),
        ],
    );
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(4);
    context.set_file_entries(vec![entry]);
    let group = Arc::new(RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7104),
            Vec::new(),
            Default::default(),
        ),
    ));
    group.recover().set_download_context(Arc::new(context));
    let manager = Arc::new(WebSeedManager::for_request_group(
        Arc::clone(&group),
        4,
        8,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    ));

    let launch = Arc::new(Barrier::new(3));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let mut requests = tokio::task::JoinSet::new();
    for piece_index in 0..2 {
        let launch = Arc::clone(&launch);
        let manager = Arc::clone(&manager);
        let started_tx = started_tx.clone();
        requests.spawn(async move {
            launch.wait().await;
            started_tx
                .send(())
                .expect("report concurrent request start");
            let data = manager
                .request_piece_with_length_and_activity(piece_index, 4, None)
                .await
                .expect("healthy mirror should serve concurrent piece");
            (piece_index, data)
        });
    }
    drop(started_tx);
    launch.wait().await;
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(5), started_rx.recv())
            .await
            .expect("both piece requests should start")
            .expect("piece request task should remain connected");
    }
    release_dead_tx
        .send(())
        .expect("release the held dead-source response");

    let mut pieces = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut pieces = Vec::new();
        while let Some(result) = requests.join_next().await {
            pieces.push(result.expect("piece request task should not panic"));
        }
        pieces
    })
    .await
    .expect("both healthy Range responses should complete concurrently");
    pieces.sort_unstable_by_key(|(piece_index, _)| *piece_index);
    assert_eq!(pieces[0], (0, b"ABCD".to_vec()));
    assert_eq!(pieces[1], (1, b"EFGH".to_vec()));
    assert_eq!(
        dead_requests.load(Ordering::Relaxed),
        1,
        "concurrent piece requests should share one unresolved 404 probe"
    );
    assert_eq!(
        healthy_requests.load(Ordering::Relaxed),
        2,
        "the healthy mirror should serve both concurrent piece ranges"
    );

    stop_dead_tx.send(()).expect("stop dead WebSeed fixture");
    dead_server
        .await
        .expect("dead WebSeed fixture should finish");
    healthy_server
        .await
        .expect("healthy WebSeed fixture should finish");
}

#[tokio::test]
async fn live_web_seed_header_timeout_releases_probe_and_falls_back() {
    let dead_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled WebSeed fixture");
    let dead_address = dead_listener.local_addr().expect("stalled WebSeed address");
    let dead_requests = Arc::new(AtomicUsize::new(0));
    let server_dead_requests = Arc::clone(&dead_requests);
    let (stop_dead_tx, mut stop_dead_rx) = oneshot::channel();
    let dead_server = tokio::spawn(async move {
        let (mut stream, _) = dead_listener
            .accept()
            .await
            .expect("accept stalled WebSeed request");
        read_request_headers(&mut stream).await;
        server_dead_requests.fetch_add(1, Ordering::Relaxed);
        let _ = (&mut stop_dead_rx).await;
    });

    let healthy_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fallback WebSeed fixture");
    let healthy_address = healthy_listener
        .local_addr()
        .expect("fallback WebSeed address");
    let (stop_healthy_tx, mut stop_healthy_rx) = oneshot::channel();
    let healthy_server = tokio::spawn(async move {
        tokio::select! {
            accepted = healthy_listener.accept() => {
                let (mut stream, _) = accepted.expect("accept fallback WebSeed request");
                let request = read_request_headers(&mut stream).await;
                let (start, end) = requested_range(&request);
                let body = b"ABCD"[start..=end].to_vec();
                stream
                    .write_all(partial_response_headers(body.len()).as_bytes())
                    .await
                    .expect("write fallback WebSeed headers");
                stream
                    .write_all(&body)
                    .await
                    .expect("write fallback WebSeed body");
            }
            _ = &mut stop_healthy_rx => {}
        }
    });

    let entry = crate::download::file_entry::FileEntry::new(
        "file.bin".into(),
        4,
        0,
        vec![
            format!("http://{dead_address}/file"),
            format!("http://{healthy_address}/file"),
        ],
    );
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(4);
    context.set_file_entries(vec![entry]);
    let options = crate::request::request_group::DownloadOptions {
        timeout: Some(1),
        ..Default::default()
    };
    let group = Arc::new(RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7105),
            Vec::new(),
            options,
        ),
    ));
    group.recover().set_download_context(Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        Arc::clone(&group),
        4,
        4,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        manager.request_piece_with_length_and_activity(0, 4, None),
    )
    .await;
    stop_dead_tx.send(()).expect("stop stalled WebSeed fixture");
    let _ = stop_healthy_tx.send(());
    dead_server
        .await
        .expect("stalled WebSeed fixture should finish");
    healthy_server
        .await
        .expect("fallback WebSeed fixture should finish");

    let data = result
        .expect("WebSeed should leave a stalled header request within the configured timeout")
        .expect("the healthy fallback should complete the piece");
    assert_eq!(data, b"ABCD");
    assert_eq!(dead_requests.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn live_web_seed_body_timeout_releases_probe_and_falls_back() {
    let dead_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled WebSeed body fixture");
    let dead_address = dead_listener
        .local_addr()
        .expect("stalled WebSeed body address");
    let dead_requests = Arc::new(AtomicUsize::new(0));
    let server_dead_requests = Arc::clone(&dead_requests);
    let (stop_dead_tx, mut stop_dead_rx) = oneshot::channel();
    let dead_server = tokio::spawn(async move {
        let (mut stream, _) = dead_listener
            .accept()
            .await
            .expect("accept stalled WebSeed body request");
        read_request_headers(&mut stream).await;
        server_dead_requests.fetch_add(1, Ordering::Relaxed);
        stream
            .write_all(partial_response_headers(4).as_bytes())
            .await
            .expect("write stalled-body WebSeed headers");
        let _ = (&mut stop_dead_rx).await;
    });

    let healthy_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind healthy fallback fixture");
    let healthy_address = healthy_listener
        .local_addr()
        .expect("healthy fallback address");
    let (stop_healthy_tx, mut stop_healthy_rx) = oneshot::channel();
    let healthy_server = tokio::spawn(async move {
        tokio::select! {
            accepted = healthy_listener.accept() => {
                let (mut stream, _) = accepted.expect("accept healthy fallback request");
                let request = read_request_headers(&mut stream).await;
                let (start, end) = requested_range(&request);
                let body = b"ABCD"[start..=end].to_vec();
                stream
                    .write_all(partial_response_headers(body.len()).as_bytes())
                    .await
                    .expect("write healthy fallback headers");
                stream
                    .write_all(&body)
                    .await
                    .expect("write healthy fallback body");
            }
            _ = &mut stop_healthy_rx => {}
        }
    });

    let entry = crate::download::file_entry::FileEntry::new(
        "file.bin".into(),
        4,
        0,
        vec![
            format!("http://{dead_address}/file"),
            format!("http://{healthy_address}/file"),
        ],
    );
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(4);
    context.set_file_entries(vec![entry]);
    let options = crate::request::request_group::DownloadOptions {
        timeout: Some(1),
        ..Default::default()
    };
    let group = Arc::new(RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7106),
            Vec::new(),
            options,
        ),
    ));
    group.recover().set_download_context(Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        Arc::clone(&group),
        4,
        4,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        manager.request_piece_with_length_and_activity(0, 4, None),
    )
    .await;
    stop_dead_tx
        .send(())
        .expect("stop stalled WebSeed body fixture");
    let _ = stop_healthy_tx.send(());
    dead_server
        .await
        .expect("stalled WebSeed body fixture should finish");
    healthy_server
        .await
        .expect("healthy fallback fixture should finish");

    let data = result
        .expect("WebSeed should leave a stalled body within the configured timeout")
        .expect("the healthy fallback should complete the piece");
    assert_eq!(data, b"ABCD");
    assert_eq!(dead_requests.load(Ordering::Relaxed), 1);
}
