use std::io::BufReader;
use std::sync::Arc;

use crate::error::{Aria2Error, FatalError, Result};

use super::ClientTlsConfig;

impl ClientTlsConfig {
    pub(crate) fn websocket_connector(&self) -> Result<tokio_tungstenite::Connector> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let versions = match self.min_tls_version.as_deref() {
            None | Some("TLSv1.1") | Some("TLSv1.2") => {
                [&rustls::version::TLS13, &rustls::version::TLS12][..].to_vec()
            }
            Some("TLSv1.3") => vec![&rustls::version::TLS13],
            Some(version) => {
                return Err(config_error(format!(
                    "Unsupported minimum TLS version '{version}'"
                )));
            }
        };
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&versions)
            .map_err(|error| config_error(format!("Invalid TLS version configuration: {error}")))?;

        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = self.ca_certificate.as_deref() {
            let bytes = std::fs::read(path).map_err(|error| {
                config_error(format!("Failed to read CA certificate '{path}': {error}"))
            })?;
            let mut reader = BufReader::new(bytes.as_slice());
            let certificates = rustls_pemfile::certs(&mut reader)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    config_error(format!("Invalid CA certificate '{path}': {error}"))
                })?;
            if certificates.is_empty() {
                return Err(config_error(format!(
                    "Invalid CA certificate '{path}': no certificates found"
                )));
            }
            for certificate in certificates {
                roots.add(certificate).map_err(|error| {
                    config_error(format!("Invalid CA certificate '{path}': {error}"))
                })?;
            }
        }

        let builder = if self.check_certificate {
            builder.with_root_certificates(roots)
        } else {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SkipServerCertificateVerification {
                    provider,
                }))
        };

        let config = match (self.certificate.as_deref(), self.private_key.as_deref()) {
            (Some(certificate), Some(private_key)) => {
                let certificate_pem = std::fs::read(certificate).map_err(|error| {
                    config_error(format!(
                        "Failed to read client certificate '{certificate}': {error}"
                    ))
                })?;
                let private_key_pem = std::fs::read(private_key).map_err(|error| {
                    config_error(format!(
                        "Failed to read client private key '{private_key}': {error}"
                    ))
                })?;
                let mut certificate_reader = BufReader::new(certificate_pem.as_slice());
                let certificates = rustls_pemfile::certs(&mut certificate_reader)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| {
                        config_error(format!(
                            "Invalid client certificate '{certificate}': {error}"
                        ))
                    })?;
                let mut key_reader = BufReader::new(private_key_pem.as_slice());
                let private_key = rustls_pemfile::private_key(&mut key_reader)
                    .map_err(|error| {
                        config_error(format!(
                            "Invalid client private key '{private_key}': {error}"
                        ))
                    })?
                    .ok_or_else(|| {
                        config_error(format!(
                            "Invalid client private key '{private_key}': no key found"
                        ))
                    })?;
                builder.with_client_auth_cert(certificates, private_key)
            }
            (Some(certificate), None) => {
                let identity = super::pkcs12::load_empty_password_identity_pem(certificate)?;
                let mut reader = BufReader::new(identity.as_slice());
                let certificates = rustls_pemfile::certs(&mut reader)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| {
                        config_error(format!("Invalid client identity '{certificate}': {error}"))
                    })?;
                let mut reader = BufReader::new(identity.as_slice());
                let private_key = rustls_pemfile::private_key(&mut reader)
                    .map_err(|error| {
                        config_error(format!("Invalid client identity '{certificate}': {error}"))
                    })?
                    .ok_or_else(|| {
                        config_error(format!(
                            "Invalid client identity '{certificate}': no private key found"
                        ))
                    })?;
                builder.with_client_auth_cert(certificates, private_key)
            }
            (None, Some(_)) => {
                return Err(config_error("private-key requires certificate".to_owned()));
            }
            (None, None) => Ok(builder.with_no_client_auth()),
        }
        .map_err(|error| config_error(format!("Invalid TLS client identity: {error}")))?;

        Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
    }
}

#[derive(Debug)]
struct SkipServerCertificateVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for SkipServerCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signed: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signed: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn config_error(message: String) -> Aria2Error {
    Aria2Error::Fatal(FatalError::Config(message))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::TlsAcceptor;
    use tokio_tungstenite::tungstenite::Message;

    use super::super::test_support::{
        TEST_CHAIN_PEM, TEST_HOST, TEST_PRIVATE_KEY_PEM, TEST_ROOT_PEM, pem_certificates,
        pem_private_key,
    };
    use super::ClientTlsConfig;

    #[tokio::test]
    async fn websocket_tracker_uses_task_custom_ca() {
        let directory = tempfile::tempdir().expect("create CA fixture directory");
        let ca_path = directory.path().join("tracker-root.pem");
        std::fs::write(&ca_path, TEST_ROOT_PEM).expect("write custom tracker CA");

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local WSS tracker");
        let address = listener.local_addr().expect("local tracker address");
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                pem_certificates(TEST_CHAIN_PEM),
                pem_private_key(TEST_PRIVATE_KEY_PEM),
            )
            .expect("configure local WSS certificate");
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept tracker client");
            let stream = acceptor
                .accept(stream)
                .await
                .expect("accept TLS connection");
            let mut websocket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept WebSocket upgrade");
            let Some(Ok(Message::Text(request))) = websocket.next().await else {
                panic!("tracker should receive an announce message");
            };
            let request: Value = serde_json::from_str(&request).expect("decode announce JSON");
            let info_hash = request["info_hash"]
                .as_str()
                .expect("announce includes info_hash");
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "action": "announce",
                        "info_hash": info_hash,
                        "peers": [{"ip": "192.0.2.9", "port": 6881}]
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send tracker response");
        });

        let connector = ClientTlsConfig {
            ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        }
        .websocket_connector()
        .expect("build task-configured WebSocket TLS connector");
        let stream = TcpStream::connect(address)
            .await
            .expect("connect to local tracker");
        let response =
            aria2_protocol::bittorrent::tracker::websocket::announce_over_stream_with_connector(
                stream,
                &format!("wss://{TEST_HOST}:{}/announce", address.port()),
                &aria2_protocol::bittorrent::tracker::websocket::WebSocketAnnounceRequest {
                    info_hash: &[0x31; 20],
                    peer_id: &[0x52; 20],
                    downloaded: 0,
                    left: 1,
                    uploaded: 0,
                    numwant: 10,
                    port: 6881,
                    event: Some("started"),
                },
                std::time::Duration::from_secs(5),
                Some(connector),
            )
            .await
            .expect("custom root should validate the local WSS tracker");

        assert_eq!(response.peers, vec!["192.0.2.9:6881".parse().unwrap()]);
        server.await.expect("local WSS tracker task");
    }
}
