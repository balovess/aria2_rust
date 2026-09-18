//! FTPS transport primitives and RFC 4217 negotiation.
//!
//! This module owns the protocol-level TLS implementation used by FTP
//! callers.  The download engine is responsible only for choosing when to
//! use it and mapping application configuration into [`FtpsConfig`].

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tracing::{debug, info, warn};

/// FTPS (FTP over TLS) configuration per RFC 4217.
///
/// The engine maps its user-facing options into this transport configuration.
/// The protocol module then applies it consistently to control and data
/// channels.
#[derive(Debug, Clone)]
pub struct FtpsConfig {
    /// Whether the caller requested an FTPS connection.
    pub enabled: bool,
    /// Verify the server certificate chain.
    ///
    /// Disabling verification is insecure and should only be used when the
    /// caller explicitly opts into it.
    pub check_certificate: bool,
    /// Optional PEM file containing trusted CA certificates.
    ///
    /// When omitted, the bundled Mozilla roots are used.
    pub ca_certificate: Option<PathBuf>,
    /// Minimum TLS protocol version to negotiate.
    pub min_tls_version: TlsVersion,
}

impl Default for FtpsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_certificate: true,
            ca_certificate: None,
            min_tls_version: TlsVersion::Tls12,
        }
    }
}

/// Minimum TLS protocol version for FTPS connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsVersion {
    /// TLS 1.2 (RFC 5246).
    #[default]
    Tls12,
    /// TLS 1.3 (RFC 8446).
    Tls13,
}

/// FTP control stream which can be plain TCP or TLS-protected.
#[derive(Debug)]
pub enum FtpControlStream {
    /// Unencrypted FTP control connection.
    Plain(TcpStream),
    /// TLS-encrypted FTP control connection.
    Tls(Box<TlsStream<TcpStream>>),
}

impl FtpControlStream {
    /// Returns whether this stream is TLS-protected.
    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }

    /// Returns the underlying TCP stream reference.
    pub fn get_ref(&self) -> Option<&TcpStream> {
        match self {
            Self::Plain(stream) => Some(stream),
            Self::Tls(stream) => Some(stream.get_ref().0),
        }
    }
}

/// FTP data stream which can be plain TCP or protected by `PROT P`.
#[derive(Debug)]
pub enum FtpDataStream {
    /// Unencrypted FTP data connection.
    Plain(TcpStream),
    /// TLS-encrypted FTP data connection.
    Tls(Box<TlsStream<TcpStream>>),
}

impl FtpDataStream {
    /// Set TCP_NODELAY on the underlying socket.
    pub fn set_nodelay(&self, enabled: bool) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_nodelay(enabled),
            Self::Tls(stream) => stream.get_ref().0.set_nodelay(enabled),
        }
    }
}

fn parse_response_code(response: &str, command: &str) -> Result<u16, String> {
    let code_bytes = response
        .as_bytes()
        .get(..3)
        .ok_or_else(|| format!("{} response too short: {}", command, response))?;
    let code_text = std::str::from_utf8(code_bytes)
        .map_err(|_| format!("Invalid {} response code: {}", command, response))?;
    code_text
        .parse()
        .map_err(|_| format!("Invalid {} response code: {}", command, response))
}

// =============================================================================
// TLS connector construction
// =============================================================================

/// Build a `tokio_rustls::TlsConnector` from an [`FtpsConfig`].
pub fn build_tls_connector(config: &FtpsConfig) -> Result<tokio_rustls::TlsConnector, String> {
    use rustls::ClientConfig;
    use rustls::crypto::ring::default_provider;

    let _ = default_provider().install_default();
    let mut root_store = rustls::RootCertStore::empty();

    if config.check_certificate {
        if let Some(ref ca_path) = config.ca_certificate {
            let certs = load_pem_certs(ca_path)?;
            let (added, rejected) = root_store.add_parsable_certificates(certs);
            if added == 0 {
                return Err(format!(
                    "No valid CA certificates found in: {} ({} rejected)",
                    ca_path.display(),
                    rejected
                ));
            }
            info!(
                "Loaded {} CA certificate(s) from {} ({} rejected)",
                added,
                ca_path.display(),
                rejected
            );
        } else {
            root_store.roots = webpki_roots::TLS_SERVER_ROOTS.to_vec();
            if root_store.is_empty() {
                return Err("No bundled root certificates available".to_string());
            }
            debug!(
                "Using {} bundled Mozilla root certificates",
                root_store.len()
            );
        }
    } else {
        warn!("Certificate verification DISABLED — FTPS connection is insecure");
    }

    let mut client_config = match config.min_tls_version {
        TlsVersion::Tls12 => ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
        TlsVersion::Tls13 => ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| format!("Failed to set TLS 1.3 only: {}", e))?
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    };

    if !config.check_certificate {
        client_config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoCertificateVerification));
    }

    Ok(tokio_rustls::TlsConnector::from(Arc::new(client_config)))
}

fn load_pem_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    use std::io::BufReader as SyncBufReader;

    let file = std::fs::File::open(path).map_err(|e| {
        format!(
            "Failed to open CA certificate file {}: {}",
            path.display(),
            e
        )
    })?;

    let mut reader = SyncBufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            format!(
                "Failed to parse CA certificates from {}: {}",
                path.display(),
                e
            )
        })?;

    if certs.is_empty() {
        return Err(format!("No PEM certificates found in {}", path.display()));
    }

    Ok(certs)
}

/// Certificate verifier used when certificate checking is explicitly disabled.
#[derive(Debug)]
pub(crate) struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// =============================================================================
// RFC 4217 control-channel negotiation
// =============================================================================

/// Perform `AUTH TLS`, the TLS handshake, `PBSZ 0`, and `PROT P`.
pub async fn upgrade_control_stream(
    mut stream: TcpStream,
    host: &str,
    config: &FtpsConfig,
) -> Result<TlsStream<TcpStream>, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    debug!("Sending AUTH TLS to {}", host);
    stream
        .write_all(b"AUTH TLS\r\n")
        .await
        .map_err(|e| format!("Failed to send AUTH TLS: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("Failed to flush AUTH TLS: {}", e))?;

    let mut reader = tokio::io::BufReader::new(stream);
    let mut line_buf = String::new();
    reader
        .read_line(&mut line_buf)
        .await
        .map_err(|e| format!("Failed to read AUTH TLS response: {}", e))?;

    let response = line_buf.trim();
    debug!("AUTH TLS response: {}", response);
    let code = parse_response_code(response, "AUTH TLS")?;
    if code != 234 {
        return Err(format!(
            "AUTH TLS rejected by server: {} (expected 234)",
            response
        ));
    }
    info!("AUTH TLS accepted (234) — proceeding with TLS handshake");

    let stream = reader.into_inner();
    let mut tls_stream = perform_tls_handshake(stream, host, config).await?;
    let mut reader = tokio::io::BufReader::new(&mut tls_stream);
    let mut line_buf = String::new();

    debug!("Sending PBSZ 0");
    reader
        .get_mut()
        .write_all(b"PBSZ 0\r\n")
        .await
        .map_err(|e| format!("Failed to send PBSZ 0: {}", e))?;
    reader
        .get_mut()
        .flush()
        .await
        .map_err(|e| format!("Failed to flush PBSZ 0: {}", e))?;
    line_buf.clear();
    reader
        .read_line(&mut line_buf)
        .await
        .map_err(|e| format!("Failed to read PBSZ response: {}", e))?;
    let pbsz_resp = line_buf.trim();
    debug!("PBSZ response: {}", pbsz_resp);
    let pbsz_code = parse_response_code(pbsz_resp, "PBSZ")?;
    if pbsz_code != 200 {
        return Err(format!(
            "PBSZ 0 rejected by server: {} (expected 200)",
            pbsz_resp
        ));
    }

    debug!("Sending PROT P");
    reader
        .get_mut()
        .write_all(b"PROT P\r\n")
        .await
        .map_err(|e| format!("Failed to send PROT P: {}", e))?;
    reader
        .get_mut()
        .flush()
        .await
        .map_err(|e| format!("Failed to flush PROT P: {}", e))?;
    line_buf.clear();
    reader
        .read_line(&mut line_buf)
        .await
        .map_err(|e| format!("Failed to read PROT response: {}", e))?;
    let prot_resp = line_buf.trim();
    debug!("PROT response: {}", prot_resp);
    let prot_code = parse_response_code(prot_resp, "PROT")?;
    if prot_code != 200 {
        return Err(format!(
            "PROT P rejected by server: {} (data channel would be unencrypted)",
            prot_resp
        ));
    }

    info!("PBSZ 0 and PROT P accepted — data channel will be TLS-protected");
    Ok(tls_stream)
}

/// Perform a TLS handshake on an FTP control or implicit-FTPS connection.
pub async fn perform_tls_handshake(
    stream: TcpStream,
    host: &str,
    config: &FtpsConfig,
) -> Result<TlsStream<TcpStream>, String> {
    let connector = build_tls_connector(config)?;
    let server_name: ServerName<'static> = ServerName::try_from(host.to_string())
        .map_err(|e| format!("Invalid FTPS server name '{}': {:?}", host, e))?;

    debug!("Starting TLS handshake with {}", host);
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| format!("FTPS TLS handshake failed for {}: {}", host, e))?;
    info!("TLS handshake completed successfully with {}", host);
    Ok(tls_stream)
}

/// Upgrade a `PROT P` FTP data connection to TLS.
pub async fn upgrade_data_stream(
    stream: TcpStream,
    host: &str,
    config: &FtpsConfig,
) -> Result<TlsStream<TcpStream>, String> {
    debug!("Upgrading FTP data connection to TLS for {}", host);
    let tls_stream = perform_tls_handshake(stream, host, config).await?;
    info!("FTPS data connection TLS handshake completed with {}", host);
    Ok(tls_stream)
}

impl AsyncRead for FtpControlStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for FtpControlStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for FtpDataStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for FtpDataStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftps_config_default() {
        let config = FtpsConfig::default();
        assert!(!config.enabled);
        assert!(config.check_certificate);
        assert!(config.ca_certificate.is_none());
        assert_eq!(config.min_tls_version, TlsVersion::Tls12);
    }

    #[test]
    fn tls_version_default() {
        assert_eq!(TlsVersion::default(), TlsVersion::Tls12);
    }

    #[test]
    fn response_code_requires_ascii_three_digit_prefix() {
        assert_eq!(parse_response_code("200 OK", "TEST").unwrap(), 200);
        assert!(parse_response_code("20", "TEST").is_err());
        assert!(parse_response_code("é00 OK", "TEST").is_err());
        assert!(parse_response_code("ABC OK", "TEST").is_err());
    }

    #[test]
    fn connector_builds_with_check_enabled() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: true,
            ca_certificate: None,
            min_tls_version: TlsVersion::Tls12,
        };
        assert!(build_tls_connector(&config).is_ok());
    }

    #[test]
    fn connector_builds_with_check_disabled() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: false,
            ca_certificate: None,
            min_tls_version: TlsVersion::Tls12,
        };
        assert!(build_tls_connector(&config).is_ok());
    }

    #[test]
    fn connector_rejects_missing_ca_file() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: true,
            ca_certificate: Some(PathBuf::from("/nonexistent/ca.pem")),
            min_tls_version: TlsVersion::Tls12,
        };
        assert!(build_tls_connector(&config).is_err());
    }

    #[test]
    fn connector_builds_with_tls13() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: true,
            ca_certificate: None,
            min_tls_version: TlsVersion::Tls13,
        };
        assert!(build_tls_connector(&config).is_ok());
    }
}
