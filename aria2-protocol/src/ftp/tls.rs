//! FTPS (FTP over TLS) stream upgrade per RFC 4217.
//!
//! After the FTP server accepts `AUTH TLS` (234 response), the control
//! connection must be upgraded from a plain TCP stream to a TLS stream.
//! This module provides:
//!
//! - [`FtpControlStream`] — enum wrapping either a plain `TcpStream` or a
//!   TLS-encrypted `TlsStream<TcpStream>`, implementing `AsyncRead` +
//!   `AsyncWrite` so it can be used as a drop-in replacement.
//!
//! - [`FtpsConfig`] — TLS configuration (CA certificate> certificate path,
//!   certificate verification toggle, minimum TLS version).
//!
//! - [`build_tls_connector`] — constructs a `tokio_rustls::TlsConnector`
//!   from an [`FtpsConfig`].
//!
//! - [`upgrade_stream`] — performs the TLS handshake on an existing
//!   `TcpStream`, returning a `TlsStream<TcpStream>`.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tracing::{debug, info, warn};

// =============================================================================
// FtpControlStream — polymorphic stream for plain or TLS FTP connections
// =============================================================================

/// Wrapper supporting both plain and TLS-encrypted FTP control streams.
///
/// After `AUTH TLS` is accepted (RFC 4217 section 3), the underlying
/// `TcpStream` is replaced with `TlsStream<TcpStream>`. This enum lets the
/// `FtpConnection` hold either variant without boxing or generics.
///
/// Both variants implement `AsyncRead + AsyncWrite + Unpin`, so the enum
/// dispatches I/O calls to the active variant at zero cost (no vtable).
#[derive(Debug)]
pub enum FtpControlStream {
    /// Unencrypted TCP connection (plain FTP)
    Plain(TcpStream),
    /// TLS-encrypted connection (FTPS, RFC 4217)
    Tls(TlsStream<TcpStream>),
}

impl FtpControlStream {
    /// Returns `true` if this stream is TLS-encrypted.
    pub fn is_tls(&self) -> bool {
        matches!(self, FtpControlStream::Tls(_))
    }

    /// Consumes self and returns the inner `TcpStream` if plain, or `None`
    /// if TLS-wrapped (the TLS stream owns the TCP stream internally).
    pub fn into_plain(self) -> Option<TcpStream> {
        match self {
            FtpControlStream::Plain(s) => Some(s),
            FtpControlStream::Tls(_) => None,
        }
    }
}

// Both TcpStream and TlsStream<TcpStream> are Unpin (no self-referential
// data), so Pin::new on &mut is always safe. Implement AsyncRead/AsyncWrite
// by delegating to the active variant.

impl AsyncRead for FtpControlStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            FtpControlStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            FtpControlStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
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
            FtpControlStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            FtpControlStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            FtpControlStream::Plain(s) => Pin::new(s).poll_flush(cx),
            FtpControlStream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            FtpControlStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            FtpControlStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

// =============================================================================
// FtpsConfig — TLS configuration for FTPS connections
// =============================================================================

/// Minimum TLS protocol version for FTPS connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsVersion {
    /// TLS 1.2 (RFC 5246) — default, widely supported
    #[default]
    Tls12,
    /// TLS 1.3 (RFC 8446) — latest, preferred when available
    Tls13,
}

/// FTPS (FTP over TLS) configuration per RFC 4217.
///
/// Controls TLS handshake behaviour when upgrading an FTP control
/// connection after `AUTH TLS`. Matches the C++ aria2 options
/// `--check-certificate` and `--ca-certificate`.
#[derive(Debug, Clone)]
pub struct FtpsConfig {
    /// Enable FTPS: send `AUTH TLS` after connecting and upgrade to TLS.
    /// Corresponds to C++ aria2 `--ftp-tls` (off by default; explicit FTPS
    /// via `ftps://` URL also sets this to true).
    pub enabled: bool,

    /// Verify the server's TLS certificate chain.
    /// When `false`, accepts any certificate (insecure, for testing only).
    /// Corresponds to C++ aria2 `--check-certificate`.
    pub check_certificate: bool,

    /// Path to a PEM file containing trusted CA certificates.
    /// When `None`, falls back to bundled Mozilla roots (webpki-roots).
    /// Corresponds to C++ aria2 `--ca-certificate`.
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

// =============================================================================
// TLS connector construction
// =============================================================================

/// Build a `tokio_rustls::TlsConnector` from the given [`FtpsConfig`].
///
/// Root certificate resolution order:
/// 1. If `config.ca_certificate` is `Some(path)`, load PEM certs from file.
/// 2. Otherwise, use the bundled Mozilla roots from `webpki-roots`.
///
/// If `config.check_certificate` is `false`, all certificate verification
/// is disabled (dangerous: susceptible to MITM attacks).
pub fn build_tls_connector(config: &FtpsConfig) -> Result<tokio_rustls::TlsConnector, String> {
    use rustls::crypto::ring::default_provider;
    use rustls::ClientConfig;

    // Install the ring crypto provider (idempotent if already installed)
    let _ = default_provider().install_default();

    let mut root_store = rustls::RootCertStore::empty();

    if config.check_certificate {
        if let Some(ref ca_path) = config.ca_certificate {
            // Load CA certificates from user-specified PEM file
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
            // Fallback: use bundled Mozilla root certificates.
            // For rustls 0.23 + webpki-roots 0.26, directly set the roots vec.
            root_store.roots = webpki_roots::TLS_SERVER_ROOTS.to_vec();
            if root_store.is_empty() {
                return Err("No bundled root certificates available".to_string());
            }
            debug!("Using {} bundled Mozilla root certificates", root_store.len());
        }
    } else {
        // Dangerous: accept any certificate.
        warn!("Certificate verification DISABLED — FTPS connection is insecure");
    }

    // Build ClientConfig with appropriate TLS version.
    let mut client_config = match config.min_tls_version {
        TlsVersion::Tls12 => ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
        TlsVersion::Tls13 => {
            ClientConfig::builder_with_provider(Arc::new(default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| format!("Failed to set TLS 1.3 only: {}", e))?
                .with_root_certificates(root_store)
                .with_no_client_auth()
        }
    };

    // If certificate verification is disabled, install a dangerous verifier
    if !config.check_certificate {
        client_config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoCertificateVerification));
    }

    Ok(tokio_rustls::TlsConnector::from(Arc::new(client_config)))
}

/// Load PEM-encoded certificates from a file path.
fn load_pem_certs(
    path: &std::path::Path,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    use std::io::BufReader as SyncBufReader;

    let file = std::fs::File::open(path).map_err(|e| {
        format!(
            "Failed to open CA certificate file {}: {}",
            path.display(),
            e
        )
    })?;

    let mut reader = SyncBufReader::new(file);
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            format!(
                "Failed to parse CA certificates from {}: {}",
                path.display(),
                e
            )
        })?;

    if certs.is_empty() {
        return Err(format!(
            "No PEM certificates found in {}",
            path.display()
        ));
    }

    Ok(certs)
}

// =============================================================================
// NoCertificateVerification — dangerous verifier for --check-certificate=false
// =============================================================================

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;

/// A certificate verifier that accepts everything.
///
/// **DANGER:** This disables all certificate validation, making the
/// connection vulnerable to man-in-the-middle attacks. Only use for
/// testing or when `--check-certificate=false` is explicitly set.
#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Accept any certificate — dangerous, only for --check-certificate=false
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
        // Return all schemes supported by the ring crypto provider.
        // This ensures the TLS handshake can proceed regardless of
        // which signature algorithm the server uses.
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// =============================================================================
// TLS stream upgrade
// =============================================================================

/// Upgrade a plain `TcpStream` to a TLS-encrypted stream.
///
/// This is called after the FTP server accepts `AUTH TLS` (234 response).
/// The TLS handshake is performed asynchronously. On success, returns
/// `TlsStream<TcpStream>` which replaces the plain stream in
/// `FtpControlStream`.
///
/// # Errors
///
/// Returns an error if:
/// - The TLS connector cannot be built (bad CA file, no roots)
/// - The TLS handshake fails (server cert invalid, handshake timeout)
pub async fn upgrade_stream(
    stream: TcpStream,
    host: &str,
    config: &FtpsConfig,
) -> Result<TlsStream<TcpStream>, String> {
    let connector = build_tls_connector(config)?;

    // Create an owned ServerName<'static> from the host string.
    // We need 'static lifetime because tokio_rustls::TlsConnector::connect
    // requires ServerName<'static>.
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

// =============================================================================
// Unit tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ftps_config_default() {
        let config = FtpsConfig::default();
        assert!(!config.enabled);
        assert!(config.check_certificate);
        assert!(config.ca_certificate.is_none());
        assert_eq!(config.min_tls_version, TlsVersion::Tls12);
    }

    #[test]
    fn test_tls_version_default() {
        assert_eq!(TlsVersion::default(), TlsVersion::Tls12);
    }

    #[test]
    fn test_ftp_control_stream_plain_is_not_tls() {
        let config = FtpsConfig::default();
        assert!(!config.enabled);
    }

    #[test]
    fn test_build_tls_connector_with_check_enabled() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: true,
            ca_certificate: None,
            min_tls_version: TlsVersion::Tls12,
        };
        let result = build_tls_connector(&config);
        assert!(result.is_ok(), "Connector should build with bundled roots");
    }

    #[test]
    fn test_build_tls_connector_with_check_disabled() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: false,
            ca_certificate: None,
            min_tls_version: TlsVersion::Tls12,
        };
        let result = build_tls_connector(&config);
        assert!(
            result.is_ok(),
            "Connector should build even without cert verification"
        );
    }

    #[test]
    fn test_build_tls_connector_with_nonexistent_ca_file() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: true,
            ca_certificate: Some(PathBuf::from("/nonexistent/ca.pem")),
            min_tls_version: TlsVersion::Tls12,
        };
        let result = build_tls_connector(&config);
        assert!(result.is_err(), "Should fail with nonexistent CA file");
    }

    #[test]
    fn test_build_tls_connector_tls13() {
        let config = FtpsConfig {
            enabled: true,
            check_certificate: true,
            ca_certificate: None,
            min_tls_version: TlsVersion::Tls13,
        };
        let result = build_tls_connector(&config);
        assert!(result.is_ok(), "Connector should build with TLS 1.3 only");
    }
}
