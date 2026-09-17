use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::convert::Infallible;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

pub(super) const TEST_HOST: &str = "foobar.com";
pub(super) const TEST_ROOT_PEM: &str = include_str!("../testdata/rustls_root.pem");
pub(super) const TEST_CHAIN_PEM: &str = include_str!("../testdata/rustls_chain.pem");
pub(super) const TEST_PRIVATE_KEY_PEM: &str = include_str!("../testdata/rustls_end.key");
pub(super) const TLS12_ONLY: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS12];

pub(super) struct HttpsFixture {
    pub(super) address: SocketAddr,
    task: JoinHandle<()>,
}

impl HttpsFixture {
    pub(super) fn url(&self) -> String {
        format!("https://{}:{}/payload", TEST_HOST, self.address.port())
    }
}

impl Drop for HttpsFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn pem_certificates(pem: &str) -> Vec<CertificateDer<'static>> {
    rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("test certificate PEM should parse")
}

pub(super) fn pem_private_key(pem: &str) -> PrivateKeyDer<'static> {
    rustls_pemfile::private_key(&mut BufReader::new(pem.as_bytes()))
        .expect("test private key PEM should parse")
        .expect("test private key should exist")
}

fn test_server_config_with_versions(
    require_client_auth: bool,
    versions: &'static [&'static rustls::SupportedProtocolVersion],
) -> rustls::ServerConfig {
    let certificates = pem_certificates(TEST_CHAIN_PEM);
    let private_key = pem_private_key(TEST_PRIVATE_KEY_PEM);

    if require_client_auth {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in pem_certificates(TEST_ROOT_PEM) {
            roots
                .add(certificate)
                .expect("test root should be accepted");
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .expect("test client verifier should build");

        rustls::ServerConfig::builder_with_protocol_versions(versions)
            .with_client_cert_verifier(verifier)
            .with_single_cert(certificates, private_key)
            .expect("test server certificate should match its key")
    } else {
        rustls::ServerConfig::builder_with_protocol_versions(versions)
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .expect("test server certificate should match its key")
    }
}

pub(super) async fn start_https_fixture(require_client_auth: bool) -> HttpsFixture {
    start_https_fixture_with_versions(require_client_auth, rustls::ALL_VERSIONS).await
}

pub(super) async fn start_https_fixture_with_versions(
    require_client_auth: bool,
    versions: &'static [&'static rustls::SupportedProtocolVersion],
) -> HttpsFixture {
    crate::http::client_pool::ensure_rustls_provider();
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("test HTTPS listener should bind");
    let address = listener
        .local_addr()
        .expect("test listener should have an address");
    let acceptor = TlsAcceptor::from(Arc::new(test_server_config_with_versions(
        require_client_auth,
        versions,
    )));

    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(stream) = acceptor.accept(stream).await else {
            return;
        };

        let service = service_fn(|_request: Request<Incoming>| async {
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"tls-ok"))))
        });
        let _ = http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });

    HttpsFixture { address, task }
}

pub(super) fn test_client_builder(address: SocketAddr) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .resolve(TEST_HOST, address)
        .timeout(Duration::from_secs(5))
}
