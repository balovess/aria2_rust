//! End-to-end HTTP/2 Range download through the production DownloadCommand.

mod e2e_helpers;

use aria2_core::engine::command::Command;
use aria2_core::engine::http::download_command::DownloadCommand;
use aria2_core::http::HttpVersion;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::CertificateDer;
use std::convert::Infallible;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use crate::e2e_helpers::mock_http_server::{Body, full_body};

const HTTP2_TEST_SESSIONS: usize = 16;

#[tokio::test]
async fn download_command_downloads_over_multiplexed_http2_ranges() {
    aria2_core::http::client_pool::ensure_rustls_provider();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let data = Arc::new(
        (0..32 * 1024 * 1024)
            .map(|index| (index as u8).wrapping_mul(37).wrapping_add(11))
            .collect::<Vec<_>>(),
    );

    let certificates = rustls_pemfile::certs(&mut BufReader::new(
        include_str!("../src/http/testdata/rustls_chain.pem").as_bytes(),
    ))
    .collect::<std::result::Result<Vec<CertificateDer<'static>>, _>>()
    .unwrap();
    let private_key = rustls_pemfile::private_key(&mut BufReader::new(
        include_str!("../src/http/testdata/rustls_end.key").as_bytes(),
    ))
    .unwrap()
    .unwrap();
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .unwrap();
    server_config.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let accepted_connections = Arc::new(AtomicUsize::new(0));
    let payload_requests = Arc::new(AtomicUsize::new(0));
    let h2_payload_requests = Arc::new(AtomicUsize::new(0));
    let first_range_attempts = Arc::new(AtomicUsize::new(0));
    let active_payload_streams = Arc::new(AtomicUsize::new(0));
    let peak_payload_streams = Arc::new(AtomicUsize::new(0));
    let per_connection_active = Arc::new(
        (0..HTTP2_TEST_SESSIONS)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let per_connection_peak = Arc::new(
        (0..HTTP2_TEST_SESSIONS)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );

    let server_accepted_connections = Arc::clone(&accepted_connections);
    let server_payload_requests = Arc::clone(&payload_requests);
    let server_h2_payload_requests = Arc::clone(&h2_payload_requests);
    let server_first_range_attempts = Arc::clone(&first_range_attempts);
    let server_active_payload_streams = Arc::clone(&active_payload_streams);
    let server_peak_payload_streams = Arc::clone(&peak_payload_streams);
    let server_per_connection_active = Arc::clone(&per_connection_active);
    let server_per_connection_peak = Arc::clone(&per_connection_peak);
    let server_data = Arc::clone(&data);
    let server = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let connection_index = server_accepted_connections.fetch_add(1, Ordering::AcqRel);
            let acceptor = acceptor.clone();
            let data = Arc::clone(&server_data);
            let payload_requests = Arc::clone(&server_payload_requests);
            let h2_payload_requests = Arc::clone(&server_h2_payload_requests);
            let first_range_attempts = Arc::clone(&server_first_range_attempts);
            let active_payload_streams = Arc::clone(&server_active_payload_streams);
            let peak_payload_streams = Arc::clone(&server_peak_payload_streams);
            let per_connection_active = Arc::clone(&server_per_connection_active);
            let per_connection_peak = Arc::clone(&server_per_connection_peak);
            connections.spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(move |request: Request<Incoming>| {
                    let data = Arc::clone(&data);
                    let payload_requests = Arc::clone(&payload_requests);
                    let h2_payload_requests = Arc::clone(&h2_payload_requests);
                    let first_range_attempts = Arc::clone(&first_range_attempts);
                    let active_payload_streams = Arc::clone(&active_payload_streams);
                    let peak_payload_streams = Arc::clone(&peak_payload_streams);
                    let per_connection_active = Arc::clone(&per_connection_active);
                    let per_connection_peak = Arc::clone(&per_connection_peak);
                    async move {
                        if request.method() == hyper::Method::HEAD {
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header(ACCEPT_RANGES, "bytes")
                                    .header(CONTENT_LENGTH, data.len())
                                    .body(full_body(Bytes::new()))
                                    .unwrap(),
                            );
                        }

                        let range = request
                            .headers()
                            .get(RANGE)
                            .and_then(|value| value.to_str().ok())
                            .unwrap();
                        let (start, end) = range
                            .strip_prefix("bytes=")
                            .unwrap()
                            .split_once('-')
                            .unwrap();
                        let start = start.parse::<usize>().unwrap();
                        let end = end.parse::<usize>().unwrap();
                        assert!(start <= end && end < data.len());
                        let is_payload = range != "bytes=0-0";
                        let slow_first_attempt = is_payload
                            && start == 0
                            && first_range_attempts.fetch_add(1, Ordering::AcqRel) == 0;
                        if is_payload {
                            payload_requests.fetch_add(1, Ordering::Relaxed);
                            if request.version() == Version::HTTP_2 {
                                h2_payload_requests.fetch_add(1, Ordering::Relaxed);
                            }
                            let current = active_payload_streams.fetch_add(1, Ordering::AcqRel) + 1;
                            peak_payload_streams.fetch_max(current, Ordering::AcqRel);
                            if let (Some(active), Some(peak)) = (
                                per_connection_active.get(connection_index),
                                per_connection_peak.get(connection_index),
                            ) {
                                let active_count = active.fetch_add(1, Ordering::AcqRel) + 1;
                                peak.fetch_max(active_count, Ordering::AcqRel);
                            }
                            // Keep responses in flight long enough to observe actual H2 multiplexing.
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            active_payload_streams.fetch_sub(1, Ordering::AcqRel);
                            if let Some(active) = per_connection_active.get(connection_index) {
                                active.fetch_sub(1, Ordering::AcqRel);
                            }
                        }

                        let response_body: Body = if slow_first_attempt {
                            let range = Arc::new(data[start..=end].to_vec());
                            let stream = futures::stream::unfold(0usize, move |offset| {
                                let range = Arc::clone(&range);
                                async move {
                                    if offset >= range.len() {
                                        return None;
                                    }
                                    tokio::time::sleep(Duration::from_millis(250)).await;
                                    let next = (offset + 32 * 1024).min(range.len());
                                    Some((
                                        Ok::<_, Infallible>(Frame::data(Bytes::copy_from_slice(
                                            &range[offset..next],
                                        ))),
                                        next,
                                    ))
                                }
                            });
                            StreamBody::new(stream).boxed()
                        } else {
                            full_body(Bytes::copy_from_slice(&data[start..=end]))
                        };

                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::PARTIAL_CONTENT)
                                .header(
                                    CONTENT_RANGE,
                                    format!("bytes {start}-{end}/{}", data.len()),
                                )
                                .header(CONTENT_LENGTH, end - start + 1)
                                .body(response_body)
                                .unwrap(),
                        )
                    }
                });
                let _ = http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let output_name = "http2-range.bin";
    let options = DownloadOptions {
        split: Some(16),
        max_connection_per_server: Some(4),
        http_version: HttpVersion::Http2,
        use_head: true,
        check_certificate: false,
        dir: Some(dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_owned()),
        ..Default::default()
    };
    let uri = format!("https://127.0.0.1:{}/file", address.port());
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(7301),
        vec![uri.clone()],
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    let mut command = DownloadCommand::new_with_group(
        group,
        &uri,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some(output_name),
    )
    .unwrap();

    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(30), command.execute())
        .await
        .expect("HTTP/2 DownloadCommand should finish within 30 seconds")
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the slow H2 Range should be replaced before its 8-second body completes: {:?}",
        started.elapsed()
    );
    assert!(
        first_range_attempts.load(Ordering::Acquire) >= 2,
        "the slow H2 Range must be requested again"
    );
    let output = std::fs::read(dir.path().join(output_name)).unwrap();
    assert_eq!(output.as_slice(), data.as_slice());
    assert_eq!(
        accepted_connections.load(Ordering::Relaxed),
        4,
        "the accepted H2 server should keep the initial four-session ceiling"
    );
    assert!(payload_requests.load(Ordering::Relaxed) >= 4);
    assert_eq!(
        h2_payload_requests.load(Ordering::Relaxed),
        payload_requests.load(Ordering::Relaxed),
        "every payload Range must be served on negotiated HTTP/2"
    );
    assert!(
        peak_payload_streams.load(Ordering::Relaxed) >= 4,
        "the H2 controller should start with four fixed streams on one connection"
    );
    assert!(
        per_connection_peak
            .iter()
            .any(|peak| peak.load(Ordering::Relaxed) >= 4),
        "one physical TCP connection should carry the four initial H2 streams"
    );
    assert!(
        per_connection_peak
            .iter()
            .all(|peak| peak.load(Ordering::Relaxed) <= 4),
        "the default four-stream limit must remain fixed on every H2 connection"
    );

    drop(command);
    server.abort();
}
