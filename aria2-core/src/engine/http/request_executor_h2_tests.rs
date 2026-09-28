use super::*;

#[tokio::test]
async fn concurrent_ranges_warm_and_multiplex_across_four_http2_sessions() {
    use std::convert::Infallible;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::{
        Request, Response,
        body::Incoming,
        header::{CONTENT_RANGE, RANGE},
        server::conn::http2,
        service::service_fn,
    };
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tokio::net::TcpListener;

    const SESSION_COUNT: usize = 4;
    const SEGMENT_COUNT: usize = 16;
    const SEGMENT_SIZE: usize = 4;
    const TOTAL_SIZE: usize = SEGMENT_COUNT * SEGMENT_SIZE;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let expected = Bytes::from((0..TOTAL_SIZE).map(|index| index as u8).collect::<Vec<_>>());
    let server_expected = expected.clone();
    let served_per_connection = Arc::new(
        (0..SESSION_COUNT)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let server_served_per_connection = Arc::clone(&served_per_connection);
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        for connection_index in 0..SESSION_COUNT {
            let (stream, _) = listener.accept().await.unwrap();
            let body = server_expected.clone();
            let served_per_connection = Arc::clone(&server_served_per_connection);
            let service = service_fn(move |request: Request<Incoming>| {
                let body = body.clone();
                let served_per_connection = Arc::clone(&served_per_connection);
                async move {
                    let range = request.headers().get(RANGE).unwrap().to_str().unwrap();
                    let (start, end) = range
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    let start = start.parse::<usize>().unwrap();
                    let end = end.parse::<usize>().unwrap();
                    assert!(start <= end && end < TOTAL_SIZE);
                    served_per_connection[connection_index].fetch_add(1, AtomicOrdering::Relaxed);
                    let response_body = body.slice(start..end + 1);
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(reqwest::StatusCode::PARTIAL_CONTENT)
                            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{TOTAL_SIZE}"))
                            .body(Full::new(response_body))
                            .unwrap(),
                    )
                }
            });
            connections.spawn(async move {
                http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .unwrap();
            });
        }
        while let Some(connection) = connections.join_next().await {
            connection.unwrap();
        }
    });

    crate::http::client_pool::ensure_rustls_provider();
    let clients = (0..SESSION_COUNT)
        .map(|_| {
            crate::http::client_pool::configure_http2_download_client(
                reqwest::Client::builder().http2_prior_knowledge(),
            )
            .build()
            .unwrap()
        })
        .collect::<Vec<_>>();
    let url = format!("http://{address}/file");
    let authority = authority_key(&url).unwrap();
    let mut executor = HttpSegmentRequestExecutor::new_with_clients(
        &clients[0],
        &clients,
        HttpRequestPolicy::default(),
        CookieHelper::new(Arc::new(crate::http::cookie::CookieStorage::new()), None),
        AuthResolveOptions::default(),
        None,
        SEGMENT_COUNT,
        std::slice::from_ref(&authority),
        SEGMENT_COUNT,
    );
    let progress = crate::engine::http::segment_downloader::SegmentProgressTracker::new(
        0,
        Arc::new(crate::request::request_group::AtomicProgress::new()),
    );
    let (write_tx, mut write_rx) = mpsc::channel(SEGMENT_COUNT);

    for segment_index in 0..SEGMENT_COUNT {
        let start = (segment_index * SEGMENT_SIZE) as u64;
        assert!(
            executor
                .try_submit(HttpSegmentRequest {
                    segment_index: segment_index as u32,
                    authority_key: authority.clone(),
                    url: url.clone(),
                    offset: start,
                    length: SEGMENT_SIZE as u64,
                    cookie_header: None,
                    progress: progress.new_segment(),
                    write_tx: write_tx.clone(),
                    expected_entity_length: TOTAL_SIZE as u64,
                })
                .is_some()
        );
    }

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for _ in 0..SEGMENT_COUNT {
            let result = executor.next_result().await.unwrap();
            assert_eq!(result.result.unwrap(), SEGMENT_SIZE as u64);
        }
        executor.shutdown().await;
    })
    .await
    .expect("all Range tasks should complete over the four warmed HTTP/2 sessions");
    drop(write_tx);

    let mut received = vec![0; TOTAL_SIZE];
    while let Some(chunk) = write_rx.recv().await {
        let start = chunk.offset as usize;
        received[start..start + chunk.data.len()].copy_from_slice(&chunk.data);
    }
    assert_eq!(received, expected);

    assert_eq!(
        served_per_connection
            .iter()
            .map(|count| count.load(AtomicOrdering::Relaxed))
            .collect::<Vec<_>>(),
        vec![
            SEGMENT_COUNT / SESSION_COUNT,
            SEGMENT_COUNT / SESSION_COUNT + 1,
            SEGMENT_COUNT / SESSION_COUNT + 1,
            SEGMENT_COUNT / SESSION_COUNT + 1,
        ],
        "the primary session is already warmed by range probing; secondary sessions receive one warmup"
    );
    drop(clients);
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("HTTP/2 sessions should shut down after the clients are dropped")
        .unwrap();
}

#[tokio::test]
async fn one_http2_client_stays_on_one_tcp_when_server_caps_streams_at_one() {
    use std::convert::Infallible;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::{
        Request, Response,
        body::Incoming,
        header::{CONTENT_RANGE, RANGE},
        server::conn::http2,
        service::service_fn,
    };
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tokio::net::TcpListener;

    const RANGE_COUNT: usize = 8;
    const TOTAL_SIZE: usize = RANGE_COUNT;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted_connections = Arc::new(AtomicUsize::new(0));
    let server_connections = Arc::clone(&accepted_connections);
    let requests_served = Arc::new(AtomicUsize::new(0));
    let server_requests = Arc::clone(&requests_served);
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let connection_index = server_connections.fetch_add(1, AtomicOrdering::Relaxed);
            let requests_served = Arc::clone(&server_requests);
            let service = service_fn(move |request: Request<Incoming>| {
                let requests_served = Arc::clone(&requests_served);
                async move {
                    let range = request.headers().get(RANGE).unwrap().to_str().unwrap();
                    let (start, end) = range
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    let start = start.parse::<usize>().unwrap();
                    let end = end.parse::<usize>().unwrap();
                    assert_eq!(start, end);
                    assert!(end < TOTAL_SIZE);
                    requests_served.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(reqwest::StatusCode::PARTIAL_CONTENT)
                            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{TOTAL_SIZE}"))
                            .body(Full::new(Bytes::from(vec![start as u8])))
                            .unwrap(),
                    )
                }
            });
            connections.spawn(async move {
                let mut builder = http2::Builder::new(TokioExecutor::new());
                builder.max_concurrent_streams(Some(1));
                builder
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .unwrap_or_else(|error| {
                        tracing::debug!(connection_index, %error, "local HTTP/2 test connection closed");
                    });
            });
        }
    });

    crate::http::client_pool::ensure_rustls_provider();
    let client = crate::http::client_pool::configure_http2_download_client(
        reqwest::Client::builder().http2_prior_knowledge(),
    )
    .build()
    .unwrap();
    let url = format!("http://{address}/file");
    let authority = authority_key(&url).unwrap();
    let mut executor = HttpSegmentRequestExecutor::new_with_clients(
        &client,
        std::slice::from_ref(&client),
        HttpRequestPolicy::default(),
        CookieHelper::new(Arc::new(crate::http::cookie::CookieStorage::new()), None),
        AuthResolveOptions::default(),
        None,
        RANGE_COUNT,
        std::slice::from_ref(&authority),
        1,
    );
    let state = executor.state.authority(&authority).unwrap();
    state.protocol.store(2, Ordering::Release);
    state.active_h2_sessions.store(1, Ordering::Release);
    state.target.store(RANGE_COUNT, Ordering::Release);
    let progress = crate::engine::http::segment_downloader::SegmentProgressTracker::new(
        0,
        Arc::new(crate::request::request_group::AtomicProgress::new()),
    );
    let (write_tx, mut write_rx) = mpsc::channel(RANGE_COUNT);

    for segment_index in 0..RANGE_COUNT {
        assert!(
            executor
                .try_submit(HttpSegmentRequest {
                    segment_index: segment_index as u32,
                    authority_key: authority.clone(),
                    url: url.clone(),
                    offset: segment_index as u64,
                    length: 1,
                    cookie_header: None,
                    progress: progress.new_segment(),
                    write_tx: write_tx.clone(),
                    expected_entity_length: TOTAL_SIZE as u64,
                })
                .is_some()
        );
    }

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for _ in 0..RANGE_COUNT {
            assert_eq!(executor.next_result().await.unwrap().result.unwrap(), 1);
        }
        executor.shutdown().await;
    })
    .await
    .expect("all ranges should complete on the single HTTP/2 transport");
    drop(write_tx);

    let mut received = vec![0; TOTAL_SIZE];
    while let Some(chunk) = write_rx.recv().await {
        let start = chunk.offset as usize;
        received[start..start + chunk.data.len()].copy_from_slice(&chunk.data);
    }
    assert_eq!(received, (0..TOTAL_SIZE as u8).collect::<Vec<_>>());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        accepted_connections.load(AtomicOrdering::Relaxed),
        1,
        "one reqwest client must not escape its physical H2 TCP session when the peer limits streams"
    );
    assert_eq!(requests_served.load(AtomicOrdering::Relaxed), RANGE_COUNT);

    drop(client);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn unavailable_http2_sessions_fall_back_to_the_primary_pool() {
    use std::convert::Infallible;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::{
        Request, Response,
        body::Incoming,
        header::{CONTENT_RANGE, RANGE},
        server::conn::http2,
        service::service_fn,
    };
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tokio::net::TcpListener;

    const SESSION_COUNT: usize = 4;
    const SEGMENT_COUNT: usize = 16;
    const SEGMENT_SIZE: usize = 4;
    const TOTAL_SIZE: usize = SEGMENT_COUNT * SEGMENT_SIZE;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let expected = Bytes::from((0..TOTAL_SIZE).map(|index| index as u8).collect::<Vec<_>>());
    let server_expected = expected.clone();
    let served_requests = Arc::new(AtomicUsize::new(0));
    let server_served_requests = Arc::clone(&served_requests);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let service = service_fn(move |request: Request<Incoming>| {
            let body = server_expected.clone();
            let served_requests = Arc::clone(&server_served_requests);
            async move {
                let range = request.headers().get(RANGE).unwrap().to_str().unwrap();
                let (start, end) = range
                    .strip_prefix("bytes=")
                    .unwrap()
                    .split_once('-')
                    .unwrap();
                let start = start.parse::<usize>().unwrap();
                let end = end.parse::<usize>().unwrap();
                assert!(start <= end && end < TOTAL_SIZE);
                served_requests.fetch_add(1, AtomicOrdering::Relaxed);
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(reqwest::StatusCode::PARTIAL_CONTENT)
                        .header(CONTENT_RANGE, format!("bytes {start}-{end}/{TOTAL_SIZE}"))
                        .body(Full::new(body.slice(start..end + 1)))
                        .unwrap(),
                )
            }
        });
        http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service)
            .await
            .unwrap();
    });

    crate::http::client_pool::ensure_rustls_provider();
    let primary = reqwest::Client::builder()
        .http2_prior_knowledge()
        .resolve_to_addrs("range.test", &[address])
        .build()
        .unwrap();
    let denied_address = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2)),
        address.port(),
    );
    let denied_clients = (1..SESSION_COUNT)
        .map(|_| {
            reqwest::Client::builder()
                .http2_prior_knowledge()
                .resolve_to_addrs("range.test", &[denied_address])
                .build()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let mut clients = vec![primary.clone()];
    clients.extend(denied_clients);
    let url = format!("http://range.test:{}/file", address.port());
    let authority = authority_key(&url).unwrap();
    let mut executor = HttpSegmentRequestExecutor::new_with_clients(
        &primary,
        &clients,
        HttpRequestPolicy::default(),
        CookieHelper::new(Arc::new(crate::http::cookie::CookieStorage::new()), None),
        AuthResolveOptions::default(),
        None,
        SEGMENT_COUNT,
        std::slice::from_ref(&authority),
        SEGMENT_COUNT,
    );
    let progress = crate::engine::http::segment_downloader::SegmentProgressTracker::new(
        0,
        Arc::new(crate::request::request_group::AtomicProgress::new()),
    );
    let (write_tx, mut write_rx) = mpsc::channel(SEGMENT_COUNT);
    for segment_index in 0..SEGMENT_COUNT {
        let start = (segment_index * SEGMENT_SIZE) as u64;
        assert!(
            executor
                .try_submit(HttpSegmentRequest {
                    segment_index: segment_index as u32,
                    authority_key: authority.clone(),
                    url: url.clone(),
                    offset: start,
                    length: SEGMENT_SIZE as u64,
                    cookie_header: None,
                    progress: progress.new_segment(),
                    write_tx: write_tx.clone(),
                    expected_entity_length: TOTAL_SIZE as u64,
                })
                .is_some()
        );
    }

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for _ in 0..SEGMENT_COUNT {
            let result = executor.next_result().await.unwrap();
            assert_eq!(result.result.unwrap(), SEGMENT_SIZE as u64);
        }
        executor.shutdown().await;
    })
    .await
    .expect("failed secondary sessions should route their ranges through the primary pool");
    drop(write_tx);

    let mut received = vec![0; TOTAL_SIZE];
    while let Some(chunk) = write_rx.recv().await {
        let start = chunk.offset as usize;
        received[start..start + chunk.data.len()].copy_from_slice(&chunk.data);
    }
    assert_eq!(received, expected);
    assert_eq!(
        served_requests.load(AtomicOrdering::Relaxed),
        SEGMENT_COUNT,
        "the primary session is already warmed by range probing and should serve every real Range request"
    );

    drop(clients);
    drop(primary);
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("primary HTTP/2 session should shut down after its client is dropped")
        .unwrap();
}
