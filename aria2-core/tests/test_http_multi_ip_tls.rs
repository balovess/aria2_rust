mod e2e_helpers;

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::BufReader;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use aria2_core::engine::command::Command;
use aria2_core::engine::http::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

const TEST_HOST: &str = "foobar.com";
const TEST_CHAIN_PEM: &str = include_str!("../src/http/testdata/rustls_chain.pem");
const TEST_KEY_PEM: &str = include_str!("../src/http/testdata/rustls_end.key");

#[derive(Debug)]
struct SniRecordingResolver {
    log: Arc<Mutex<Vec<String>>>,
    certified_key: CertifiedKey,
}

impl ResolvesServerCert for SniRecordingResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.log
            .lock()
            .expect("SNI log lock poisoned")
            .push(client_hello.server_name().unwrap_or("<absent>").to_owned());
        Some(Arc::new(self.certified_key.clone()))
    }
}

fn certificates() -> Vec<CertificateDer<'static>> {
    rustls_pemfile::certs(&mut BufReader::new(TEST_CHAIN_PEM.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .expect("test certificate PEM should parse")
}

fn private_key() -> PrivateKeyDer<'static> {
    rustls_pemfile::private_key(&mut BufReader::new(TEST_KEY_PEM.as_bytes()))
        .expect("test private key PEM should parse")
        .expect("test private key should exist")
}

fn tls_config(sni_log: Arc<Mutex<Vec<String>>>) -> rustls::ServerConfig {
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&private_key())
        .expect("test private key should be supported by ring");
    let resolver = SniRecordingResolver {
        log: sni_log,
        certified_key: CertifiedKey::new(certificates(), signing_key),
    };
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver))
}

fn parse_range(request: &Request<Incoming>, total: usize) -> Option<(usize, usize)> {
    let value = request.headers().get("Range")?.to_str().ok()?;
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    let start: usize = start.parse().ok()?;
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<usize>().ok()?.min(total - 1)
    };
    (start <= end && start < total).then_some((start, end))
}

fn start_tls_range_server(
    address: SocketAddr,
    body: Arc<Vec<u8>>,
    sni_log: Arc<Mutex<Vec<String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let listener = TcpListener::bind(address)
            .await
            .expect("test TLS listener should bind");
        let acceptor = TlsAcceptor::from(Arc::new(tls_config(Arc::clone(&sni_log))));

        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(stream) = acceptor.accept(stream).await else {
                continue;
            };
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let body = Arc::clone(&body);
                    async move {
                        let is_head = request.method() == hyper::Method::HEAD;
                        let range = parse_range(&request, body.len());
                        let (start, end, status) = match range {
                            Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
                            None => (0, body.len() - 1, StatusCode::OK),
                        };
                        let payload = if is_head {
                            Bytes::new()
                        } else {
                            Bytes::copy_from_slice(&body[start..=end])
                        };
                        let mut response = Response::builder()
                            .status(status)
                            .header("Accept-Ranges", "bytes")
                            .header("Content-Length", end - start + 1);
                        if range.is_some() {
                            response = response.header(
                                "Content-Range",
                                format!("bytes {start}-{end}/{}", body.len()),
                            );
                        }
                        Ok::<_, Infallible>(response.body(Full::new(payload)).unwrap())
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    })
}

#[tokio::test]
async fn multi_ip_https_preserves_sni_and_verifies_certificate() {
    aria2_core::http::client_pool::ensure_rustls_provider();
    let body = Arc::new(
        (0..(8 * 1024 * 1024))
            .map(|index| index as u8)
            .collect::<Vec<_>>(),
    );
    let first_sni = Arc::new(Mutex::new(Vec::new()));
    let second_sni = Arc::new(Mutex::new(Vec::new()));
    let first_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = first_listener.local_addr().unwrap().port();
    drop(first_listener);

    let first = start_tls_range_server(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
        Arc::clone(&body),
        Arc::clone(&first_sni),
    );
    let second = start_tls_range_server(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), port),
        Arc::clone(&body),
        Arc::clone(&second_sni),
    );

    let uri = format!("https://{TEST_HOST}:{port}/file");
    let resolved = vec![
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), port),
    ];
    let ca_certificate = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/http/testdata/rustls_root.pem"
    );
    let options = DownloadOptions {
        split: Some(4),
        max_connection_per_server: Some(4),
        use_multiple_ips: true,
        check_certificate: true,
        ca_certificate: Some(ca_certificate.to_string()),
        file_allocation: Some("none".into()),
        ..DownloadOptions::default()
    };
    let output_dir = tempfile::tempdir().unwrap();
    let output_name = "multi-ip-tls.bin";
    let output_dir_arg = output_dir.path().to_string_lossy().into_owned();
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(rand::random::<u64>()),
        vec![uri.clone()],
        options.clone(),
    )));
    group.recover().set_total_length(body.len() as u64);
    let mut command = DownloadCommand::new_with_group_and_resolved_addresses(
        group,
        &uri,
        &options,
        Some(&output_dir_arg),
        Some(output_name),
        Some(resolved),
    )
    .unwrap();
    command.group_mut().set_option_snapshot(HashMap::from([(
        "min-split-size".to_string(),
        serde_json::json!("1M"),
    )]));
    command.execute().await.unwrap();

    first.abort();
    second.abort();

    for (name, log) in [("first", first_sni), ("second", second_sni)] {
        let log = log.lock().unwrap();
        assert!(!log.is_empty(), "{name} TLS server saw no handshake");
        assert!(
            log.iter().all(|value| value == TEST_HOST),
            "{name} server saw unexpected SNI: {log:?}"
        );
    }
    assert_eq!(
        tokio::fs::read(output_dir.path().join(output_name))
            .await
            .unwrap(),
        body.as_slice()
    );
}
