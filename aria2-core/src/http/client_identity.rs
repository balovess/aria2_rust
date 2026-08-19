//! Rust-native HTTPS client TLS configuration.

use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadOptions;

/// TLS settings shared by core-owned HTTP clients.
///
/// This is deliberately an internal transport value. The configuration layer
/// keeps ownership of the aria2-compatible option names and defaults; this
/// type only prevents individual HTTP entry points from applying different
/// interpretations of those existing options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientTlsConfig {
    check_certificate: bool,
    ca_certificate: Option<String>,
    certificate: Option<String>,
    private_key: Option<String>,
}

impl Default for ClientTlsConfig {
    fn default() -> Self {
        Self {
            check_certificate: true,
            ca_certificate: None,
            certificate: None,
            private_key: None,
        }
    }
}

impl ClientTlsConfig {
    pub(crate) fn from_download_options(options: &DownloadOptions) -> Self {
        Self {
            check_certificate: options.check_certificate,
            ca_certificate: options.ca_certificate.clone(),
            certificate: options.certificate.clone(),
            private_key: options.private_key.clone(),
        }
    }

    pub(crate) fn requires_custom_client(&self) -> bool {
        !self.check_certificate
            || self.ca_certificate.is_some()
            || self.certificate.is_some()
            || self.private_key.is_some()
    }
}

/// Apply the existing aria2-compatible TLS settings to a reqwest builder.
///
/// The option names remain owned by the configuration layer. This helper owns
/// transport construction, so every core HTTP path applies the same
/// verification, CA loading, and client identity rules.
pub(crate) fn apply(
    builder: reqwest::ClientBuilder,
    config: &ClientTlsConfig,
) -> Result<reqwest::ClientBuilder> {
    let builder = if config.check_certificate {
        builder
    } else {
        builder.danger_accept_invalid_certs(true)
    };

    let builder = if let Some(ca_certificate) = config.ca_certificate.as_deref() {
        let ca = std::fs::read(ca_certificate).map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Failed to read CA certificate '{}': {}",
                ca_certificate, error
            )))
        })?;
        let mut pem_reader = std::io::BufReader::new(ca.as_slice());
        let parsed_certificates = rustls_pemfile::certs(&mut pem_reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Invalid CA certificate '{}': {}",
                    ca_certificate, error
                )))
            })?;
        if parsed_certificates.is_empty() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                format!(
                    "Invalid CA certificate '{}': no certificates found",
                    ca_certificate
                ),
            )));
        }
        let mut builder = builder;
        for certificate_der in parsed_certificates {
            let certificate =
                reqwest::Certificate::from_der(certificate_der.as_ref()).map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Invalid CA certificate '{}': {}",
                        ca_certificate, error
                    )))
                })?;
            builder = builder.add_root_certificate(certificate);
        }
        builder
    } else {
        builder
    };

    match (config.certificate.as_deref(), config.private_key.as_deref()) {
        (None, None) => Ok(builder),
        (Some(certificate), Some(private_key)) => {
            let identity = load_pem_identity(certificate, private_key)?;
            Ok(builder.identity(identity))
        }
        (Some(certificate), None) => {
            let identity = load_empty_password_pkcs12_identity(certificate)?;
            Ok(builder.identity(identity))
        }
        (None, Some(_)) => Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "private-key requires certificate".into(),
        ))),
    }
}

fn load_pem_identity(certificate: &str, private_key: &str) -> Result<reqwest::Identity> {
    let mut identity = std::fs::read(certificate).map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Failed to read client certificate '{}': {}",
            certificate, error
        )))
    })?;
    let key = std::fs::read(private_key).map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Failed to read client private key '{}': {}",
            private_key, error
        )))
    })?;
    identity.extend_from_slice(b"\n");
    identity.extend_from_slice(&key);
    reqwest::Identity::from_pem(&identity).map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Invalid client certificate/private key: {}",
            error
        )))
    })
}

fn load_empty_password_pkcs12_identity(certificate: &str) -> Result<reqwest::Identity> {
    let archive = std::fs::read(certificate).map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Failed to read client certificate '{}': {}",
            certificate, error
        )))
    })?;
    // p12_q3 uses debug assertions for a few malformed or unknown ASN.1
    // variants. PFX is user-provided input, so keep those assertions inside
    // the configuration boundary and report them as ordinary config errors.
    let pfx = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        p12_q3::PFX::parse(&archive)
    }))
    .map_err(|_| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Invalid empty-password PKCS#12 client identity '{}': unsupported ASN.1 variant",
            certificate
        )))
    })?
    .map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Invalid empty-password PKCS#12 client identity '{}': {error:?}",
            certificate
        )))
    })?;
    let password = empty_pkcs12_password(&pfx).ok_or_else(|| {
        Aria2Error::Fatal(crate::error::FatalError::Config(
            "Invalid empty-password PKCS#12 client identity: MAC verification failed".into(),
        ))
    })?;

    let bags = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pfx.bags(&password)))
        .map_err(|_| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Unsupported empty-password PKCS#12 encryption algorithm".into(),
            ))
        })?
        .map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Invalid empty-password PKCS#12 client identity '{}': {error:?}",
                certificate
            )))
        })?;
    let key_bag = bags
        .iter()
        .find(|bag| matches!(bag.bag, p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(_)))
        .ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Empty-password PKCS#12 client identity does not contain a private key".into(),
            ))
        })?;
    let key = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        key_bag.bag.get_key(&password)
    }))
    .map_err(|_| {
        Aria2Error::Fatal(crate::error::FatalError::Config(
            "Unsupported empty-password PKCS#12 private-key encryption algorithm".into(),
        ))
    })?
    .ok_or_else(|| {
        Aria2Error::Fatal(crate::error::FatalError::Config(
            "Unable to decrypt empty-password PKCS#12 client private key".into(),
        ))
    })?;
    let key_id = key_bag.local_key_id();

    let mut leaf = None;
    let mut chain = Vec::new();
    for bag in bags {
        let Some(cert) = bag.bag.get_x509_cert() else {
            continue;
        };
        let matches_key = key_id
            .as_deref()
            .is_some_and(|key_id| bag.local_key_id().as_deref() == Some(key_id));
        if leaf.is_none() && matches_key {
            leaf = Some(cert);
        } else {
            chain.push(cert);
        }
    }

    let (leaf, chain) = if let Some(leaf) = leaf {
        (leaf, chain)
    } else {
        let mut certificates = chain.into_iter();
        let leaf = certificates.next().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Empty-password PKCS#12 client identity does not contain a certificate".into(),
            ))
        })?;
        (leaf, certificates.collect())
    };

    let mut identity = pem_block("CERTIFICATE", &leaf);
    for certificate in chain {
        identity.extend_from_slice(&pem_block("CERTIFICATE", &certificate));
    }
    identity.extend_from_slice(&pem_block("PRIVATE KEY", &key));
    reqwest::Identity::from_pem(&identity).map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Invalid empty-password PKCS#12 client identity: {}",
            error
        )))
    })
}

fn empty_pkcs12_password(pfx: &p12_q3::PFX) -> Option<p12_q3::BmpString> {
    let with_trailing_zeros = p12_q3::BmpString::with_two_trailing_zeros("");
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pfx.verify_mac(&with_trailing_zeros)
    }))
    .unwrap_or(false)
    {
        return Some(with_trailing_zeros);
    }

    let without_trailing_zeros = p12_q3::BmpString::empty_without_trailing_zeros();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pfx.verify_mac(&without_trailing_zeros)
    }))
    .unwrap_or(false)
    .then_some(without_trailing_zeros)
}

fn pem_block(label: &str, der: &[u8]) -> Vec<u8> {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n").into_bytes();
    for chunk in encoded.as_bytes().chunks(64) {
        pem.extend_from_slice(chunk);
        pem.push(b'\n');
    }
    pem.extend_from_slice(format!("-----END {label}-----\n").as_bytes());
    pem
}

#[cfg(test)]
mod tests {
    use super::{ClientTlsConfig, apply};
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

    const TEST_HOST: &str = "foobar.com";
    const TEST_ROOT_PEM: &str = include_str!("testdata/rustls_root.pem");
    const TEST_CHAIN_PEM: &str = include_str!("testdata/rustls_chain.pem");
    const TEST_PRIVATE_KEY_PEM: &str = include_str!("testdata/rustls_end.key");

    struct HttpsFixture {
        address: SocketAddr,
        task: JoinHandle<()>,
    }

    impl HttpsFixture {
        fn url(&self) -> String {
            format!("https://{}:{}/payload", TEST_HOST, self.address.port())
        }
    }

    impl Drop for HttpsFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn pem_certificates(pem: &str) -> Vec<CertificateDer<'static>> {
        rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("test certificate PEM should parse")
    }

    fn pem_private_key(pem: &str) -> PrivateKeyDer<'static> {
        rustls_pemfile::private_key(&mut BufReader::new(pem.as_bytes()))
            .expect("test private key PEM should parse")
            .expect("test private key should exist")
    }

    fn test_server_config(require_client_auth: bool) -> rustls::ServerConfig {
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

            rustls::ServerConfig::builder()
                .with_client_cert_verifier(verifier)
                .with_single_cert(certificates, private_key)
                .expect("test server certificate should match its key")
        } else {
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certificates, private_key)
                .expect("test server certificate should match its key")
        }
    }

    async fn start_https_fixture(require_client_auth: bool) -> HttpsFixture {
        crate::http::client_pool::ensure_rustls_provider();
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test HTTPS listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let acceptor = TlsAcceptor::from(Arc::new(test_server_config(require_client_auth)));

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

    fn test_client_builder(address: SocketAddr) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .resolve(TEST_HOST, address)
            .timeout(Duration::from_secs(5))
    }

    #[test]
    fn accepts_an_unconfigured_identity() {
        crate::http::client_pool::ensure_rustls_provider();
        let builder = apply(reqwest::Client::builder(), &ClientTlsConfig::default())
            .expect("missing optional client identity should be valid");
        builder.build().expect("plain client should build");
    }

    #[test]
    fn marks_non_default_tls_settings_as_custom_client_requirements() {
        assert!(!ClientTlsConfig::default().requires_custom_client());
        assert!(
            ClientTlsConfig {
                check_certificate: false,
                ..ClientTlsConfig::default()
            }
            .requires_custom_client()
        );
        assert!(
            ClientTlsConfig {
                ca_certificate: Some("ca.pem".into()),
                ..ClientTlsConfig::default()
            }
            .requires_custom_client()
        );
    }

    #[test]
    fn accepts_disabled_certificate_verification() {
        crate::http::client_pool::ensure_rustls_provider();
        let builder = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                check_certificate: false,
                ..ClientTlsConfig::default()
            },
        )
        .expect("disabled certificate verification should be configurable");
        builder
            .build()
            .expect("client with disabled certificate verification should build");
    }

    #[test]
    fn reports_missing_ca_certificate_file() {
        let error = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                ca_certificate: Some("missing-ca.pem".into()),
                ..ClientTlsConfig::default()
            },
        )
        .expect_err("missing CA files must be rejected");
        assert!(error.to_string().contains("Failed to read CA certificate"));
    }

    #[test]
    fn reports_invalid_ca_certificate_pem() {
        let directory = tempfile::tempdir().expect("create temporary CA directory");
        let ca = directory.path().join("ca.pem");
        std::fs::write(&ca, b"not a CA certificate").expect("write invalid CA fixture");

        let error = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                ca_certificate: Some(ca.to_string_lossy().into_owned()),
                ..ClientTlsConfig::default()
            },
        )
        .expect_err("invalid CA PEM must be rejected");
        assert!(error.to_string().contains("Invalid CA certificate"));
    }

    #[test]
    fn rejects_a_non_pkcs12_certificate_when_private_key_is_omitted() {
        let directory = tempfile::tempdir().expect("create temporary identity directory");
        let certificate = directory.path().join("client.pem");
        std::fs::write(&certificate, b"not a PKCS#12 archive")
            .expect("write invalid PKCS#12 fixture");

        let error = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                certificate: Some(certificate.to_string_lossy().into_owned()),
                ..ClientTlsConfig::default()
            },
        )
        .expect_err("non-PKCS#12 certificates without a key must be rejected");
        assert!(
            error
                .to_string()
                .contains("Invalid empty-password PKCS#12 client identity")
        );
    }

    #[test]
    fn accepts_a_modern_empty_password_pkcs12_identity() {
        crate::http::client_pool::ensure_rustls_provider();
        let archive = include_bytes!("testdata/modern_empty_password_aes256.p12");
        let pfx = p12_q3::PFX::parse(archive).expect("modern PKCS#12 fixture should parse");
        assert!(matches!(
            pfx.mac_data
                .as_ref()
                .map(|mac_data| &mac_data.mac.digest_algorithm),
            Some(p12_q3::AlgorithmIdentifier::Sha2)
        ));
        let password = super::empty_pkcs12_password(&pfx)
            .expect("modern PKCS#12 fixture should verify with an empty password");
        let bags = pfx
            .bags(&password)
            .expect("modern PKCS#12 fixture bags should decrypt");
        let key_bag = bags
            .iter()
            .find_map(|bag| match &bag.bag {
                p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(key_bag) => Some(key_bag),
                _ => None,
            })
            .expect("modern PKCS#12 fixture should contain a private key");
        let p12_q3::AlgorithmIdentifier::Pbes2(params) = &key_bag.encryption_algorithm else {
            panic!("modern PKCS#12 fixture should use PBES2");
        };
        assert!(matches!(
            params.key_derivation_function.as_ref(),
            p12_q3::AlgorithmIdentifier::Pbkdf2(params)
                if matches!(
                    params.prf.as_ref(),
                    p12_q3::AlgorithmIdentifier::HmacWithSha256
                )
        ));
        assert!(matches!(
            params.encryption_scheme.as_ref(),
            p12_q3::AlgorithmIdentifier::AesCbcPad(iv) if iv.len() == 16
        ));

        let directory = tempfile::tempdir().expect("create temporary identity directory");
        let certificate = directory.path().join("modern-client.p12");
        std::fs::write(&certificate, archive).expect("write modern PKCS#12 fixture");

        let builder = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                certificate: Some(certificate.to_string_lossy().into_owned()),
                ..ClientTlsConfig::default()
            },
        )
        .expect("modern empty-password PKCS#12 identity should configure the client");
        builder
            .build()
            .expect("modern PKCS#12 client identity should build");
    }

    #[test]
    fn rejects_an_unpaired_private_key() {
        let error = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                private_key: Some("client.key".into()),
                ..ClientTlsConfig::default()
            },
        )
        .expect_err("private key without a certificate must be rejected");
        assert!(error.to_string().contains("requires certificate"));
    }

    #[test]
    fn reports_missing_certificate_file() {
        let error = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                certificate: Some("missing-client.pem".into()),
                private_key: Some("missing-client.key".into()),
                ..ClientTlsConfig::default()
            },
        )
        .expect_err("missing certificate files must be rejected");
        assert!(
            error
                .to_string()
                .contains("Failed to read client certificate")
        );
    }

    #[test]
    fn reports_invalid_certificate_and_key_pem() {
        let directory = tempfile::tempdir().expect("create temporary identity directory");
        let certificate = directory.path().join("client.pem");
        let private_key = directory.path().join("client.key");
        std::fs::write(&certificate, b"not a certificate")
            .expect("write invalid certificate fixture");
        std::fs::write(&private_key, b"not a private key").expect("write invalid key fixture");

        let error = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                certificate: certificate.to_string_lossy().into_owned().into(),
                private_key: private_key.to_string_lossy().into_owned().into(),
                ..ClientTlsConfig::default()
            },
        )
        .expect_err("invalid PEM must be rejected");
        assert!(
            error
                .to_string()
                .contains("Invalid client certificate/private key")
        );
    }

    #[tokio::test]
    async fn custom_ca_verifies_a_live_https_server() {
        let fixture = start_https_fixture(false).await;
        let directory = tempfile::tempdir().expect("create temporary CA directory");
        let ca_path = directory.path().join("root.pem");
        std::fs::write(&ca_path, TEST_ROOT_PEM).expect("write test root certificate");

        let config = ClientTlsConfig {
            ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        };
        let client = apply(test_client_builder(fixture.address), &config)
            .expect("custom CA should configure the client")
            .build()
            .expect("custom CA client should build");

        let response = client
            .get(fixture.url())
            .send()
            .await
            .expect("custom CA should verify the live server");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().await.expect("read HTTPS response"),
            Bytes::from_static(b"tls-ok")
        );
    }

    #[tokio::test]
    async fn disabled_certificate_verification_reaches_a_live_https_server() {
        let fixture = start_https_fixture(false).await;
        let config = ClientTlsConfig {
            check_certificate: false,
            ..ClientTlsConfig::default()
        };
        let client = apply(test_client_builder(fixture.address), &config)
            .expect("disabled verification should configure the client")
            .build()
            .expect("disabled verification client should build");

        let response = client
            .get(fixture.url())
            .send()
            .await
            .expect("disabled verification should reach the live server");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().await.expect("read HTTPS response"),
            Bytes::from_static(b"tls-ok")
        );
    }

    #[tokio::test]
    async fn client_certificate_and_private_key_complete_live_mutual_tls() {
        let fixture = start_https_fixture(true).await;
        let directory = tempfile::tempdir().expect("create temporary identity directory");
        let ca_path = directory.path().join("root.pem");
        let certificate_path = directory.path().join("client-chain.pem");
        let private_key_path = directory.path().join("client.key");
        std::fs::write(&ca_path, TEST_ROOT_PEM).expect("write test root certificate");
        std::fs::write(&certificate_path, TEST_CHAIN_PEM)
            .expect("write test client certificate chain");
        std::fs::write(&private_key_path, TEST_PRIVATE_KEY_PEM)
            .expect("write test client private key");

        let config = ClientTlsConfig {
            ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
            certificate: Some(certificate_path.to_string_lossy().into_owned()),
            private_key: Some(private_key_path.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        };
        let client = apply(test_client_builder(fixture.address), &config)
            .expect("client identity should configure the client")
            .build()
            .expect("mutual TLS client should build");

        let response = client
            .get(fixture.url())
            .send()
            .await
            .expect("client identity should complete mutual TLS");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().await.expect("read HTTPS response"),
            Bytes::from_static(b"tls-ok")
        );
    }

    fn passwordless_pkcs12_identity(password: &p12_q3::BmpString) -> Vec<u8> {
        let certificates = pem_certificates(TEST_CHAIN_PEM);
        let private_key = pem_private_key(TEST_PRIVATE_KEY_PEM);
        let ca_certificates = certificates
            .iter()
            .skip(1)
            .map(|certificate| certificate.as_ref())
            .collect::<Vec<_>>();

        p12_q3::PFX::new_with_cas(
            certificates[0].as_ref(),
            private_key.secret_der(),
            &ca_certificates,
            password,
            "aria2-rust-test",
        )
        .expect("passwordless PKCS#12 fixture should be generated")
        .to_der()
    }

    #[tokio::test]
    async fn passwordless_pkcs12_identity_completes_live_mutual_tls() {
        let fixture = start_https_fixture(true).await;
        let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
        let ca_path = directory.path().join("root.pem");
        let certificate_path = directory.path().join("client.p12");
        let password = p12_q3::BmpString::with_two_trailing_zeros("");
        std::fs::write(&ca_path, TEST_ROOT_PEM).expect("write test root certificate");
        std::fs::write(&certificate_path, passwordless_pkcs12_identity(&password))
            .expect("write passwordless PKCS#12 fixture");

        let config = ClientTlsConfig {
            ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
            certificate: Some(certificate_path.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        };
        let client = apply(test_client_builder(fixture.address), &config)
            .expect("passwordless PKCS#12 identity should configure the client")
            .build()
            .expect("passwordless PKCS#12 client should build");

        let response = client
            .get(fixture.url())
            .send()
            .await
            .expect("passwordless PKCS#12 identity should complete mutual TLS");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().await.expect("read HTTPS response"),
            Bytes::from_static(b"tls-ok")
        );
    }

    #[test]
    fn accepts_empty_password_without_bmp_terminator() {
        crate::http::client_pool::ensure_rustls_provider();
        let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
        let certificate_path = directory.path().join("client.p12");
        let password = p12_q3::BmpString::empty_without_trailing_zeros();
        std::fs::write(&certificate_path, passwordless_pkcs12_identity(&password))
            .expect("write passwordless PKCS#12 fixture");

        let builder = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                certificate: Some(certificate_path.to_string_lossy().into_owned()),
                ..ClientTlsConfig::default()
            },
        )
        .expect("empty password without a BMP terminator should be accepted");
        builder
            .build()
            .expect("passwordless PKCS#12 client should build");
    }
}
