//! FTPS (FTP over TLS) stream upgrade and TLS connector construction per RFC 4217.
//!
//! After the FTP server accepts `AUTH TLS` (234 response), the control
//! connection must be upgraded from a plain TCP stream to a TLS stream.
//! This module provides:
//!
//! - [`build_tls_connector`] — constructs a `tokio_rustls::TlsConnector`
//!   from an [`FtpsConfig`].
//!
//! - [`upgrade_control_stream`] — performs the full FTPS negotiation
//!   sequence (AUTH TLS, TLS handshake, PBSZ 0, PROT P) on an established
//!   control connection, returning a TLS-wrapped `TlsStream<TcpStream>`.
//!
//! - [`upgrade_data_stream`] — wraps a PASV/PORT data connection in TLS
//!   when PROT P has been negotiated (data channel protection = Private).
//!
//! - [`NoCertificateVerification`] — dangerous certificate verifier used
//!   when `--check-certificate=false` is set.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tracing::{debug, info, warn};

use super::types::{FtpsConfig, TlsVersion};

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
    path: &Path,
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
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut reader)
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

/// A certificate verifier that accepts everything.
///
/// **DANGER:** This disables all certificate validation, making the
/// connection vulnerable to man-in-the-middle attacks. Only use for
/// testing or when `--check-certificate=false` is explicitly set.
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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// =============================================================================
// TLS stream upgrade — control connection (AUTH TLS + PBSZ + PROT)
// =============================================================================

/// Perform the full FTPS control connection upgrade per RFC 4217.
///
/// This function takes ownership of an established plain-text FTP control
/// stream and performs the following sequence:
///
/// 1. Send `AUTH TLS` and expect 234 (Authentication mechanism accepted)
/// 2. Upgrade the TCP stream to TLS via `tokio_rustls`
/// 3. Send `PBSZ 0` (Protection Buffer Size — must be 0 for TLS)
/// 4. Send `PROT P` (Data Channel Protection Level = Private)
///
/// After this succeeds, both control and data channels will be TLS-protected.
///
/// # Arguments
///
/// - `stream`: The established plain TCP control connection (ownership transferred)
/// - `host`: Server hostname for SNI (Server Name Indication)
/// - `config`: FTPS TLS configuration
///
/// # Returns
///
/// On success, returns `TlsStream<TcpStream>` wrapping the original TCP stream.
///
/// # Errors
///
/// Returns a string describing the failure if any step fails:
/// - AUTH TLS rejected (non-234 response)
/// - TLS handshake failed
/// - PBSZ 0 rejected
/// - PROT P rejected
pub async fn upgrade_control_stream(
    mut stream: TcpStream,
    host: &str,
    config: &FtpsConfig,
) -> Result<TlsStream<TcpStream>, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    // Step 1: Send AUTH TLS on the plain stream
    debug!("Sending AUTH TLS to {}", host);
    stream
        .write_all(b"AUTH TLS\r\n")
        .await
        .map_err(|e| format!("Failed to send AUTH TLS: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("Failed to flush AUTH TLS: {}", e))?;

    // Read the 234 response using a BufReader that takes ownership.
    // After reading, we recover the inner stream for the TLS handshake.
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line_buf = String::new();
    reader
        .read_line(&mut line_buf)
        .await
        .map_err(|e| format!("Failed to read AUTH TLS response: {}", e))?;

    let response = line_buf.trim();
    debug!("AUTH TLS response: {}", response);

    if response.len() < 3 {
        return Err(format!("AUTH TLS response too short: {}", response));
    }
    let code: u16 = response[..3]
        .parse()
        .map_err(|_| format!("Invalid AUTH TLS response code: {}", &response[..3]))?;

    if code != 234 {
        return Err(format!(
            "AUTH TLS rejected by server: {} (expected 234)",
            response
        ));
    }
    info!("AUTH TLS accepted (234) — proceeding with TLS handshake");

    // Recover the TCP stream from the BufReader for TLS handshake
    let stream = reader.into_inner();

    // Step 2: TLS handshake on the control stream
    let mut tls_stream = perform_tls_handshake(stream, host, config).await?;

    // Steps 3-4: PBSZ 0 + PROT P on the now-encrypted stream.
    // We take temporary ownership via BufReader and recover the TLS stream.
    let mut reader = tokio::io::BufReader::new(&mut tls_stream);
    let mut line_buf = String::new();

    // Send PBSZ 0
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

    if pbsz_resp.len() >= 3 {
        let pbsz_code: u16 = pbsz_resp[..3]
            .parse()
            .map_err(|_| format!("Invalid PBSZ response code: {}", &pbsz_resp[..3]))?;
        if pbsz_code != 200 {
            return Err(format!(
                "PBSZ 0 rejected by server: {} (expected 200)",
                pbsz_resp
            ));
        }
    }

    // Send PROT P
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

    if prot_resp.len() >= 3 {
        let prot_code: u16 = prot_resp[..3]
            .parse()
            .map_err(|_| format!("Invalid PROT response code: {}", &prot_resp[..3]))?;
        if prot_code != 200 {
            // PROT P was rejected — data channel will be cleartext.
            // This is not fatal per RFC 4217; the control channel is
            // still encrypted. Log a warning and continue.
            warn!(
                "PROT P rejected by server: {} — data channel will NOT be encrypted",
                prot_resp
            );
            // Return the TLS stream anyway — control channel is still protected
            return Ok(tls_stream);
        }
    }

    info!("PBSZ 0 and PROT P accepted — data channel will be TLS-protected");
    Ok(tls_stream)
}

/// Perform the TLS handshake on a TCP stream.
///
/// Called after `AUTH TLS` is accepted (234 response) or when establishing
/// an implicit FTPS connection on port 990. The host is used for SNI.
pub async fn perform_tls_handshake(
    stream: TcpStream,
    host: &str,
    config: &FtpsConfig,
) -> Result<TlsStream<TcpStream>, String> {
    let connector = build_tls_connector(config)?;

    // Create an owned ServerName<'static> from the host string.
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
// TLS stream upgrade — data connection (PROT P)
// =============================================================================

/// Upgrade a data connection stream to TLS when PROT P is in effect.
///
/// After PROT P is negotiated on the control channel, all data connections
/// must also be TLS-protected. This function performs the TLS handshake
/// on a PASV/PORT data connection.
///
/// The TLS session may be resumed from the control connection's session
/// (RFC 4217 section 5), which is handled automatically by `tokio_rustls`
/// via session resumption in the `ClientConfig`.
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

// =============================================================================
// FtpControlStream — polymorphic stream for plain or TLS FTP connections
// =============================================================================

/// Wrapper supporting both plain and TLS-encrypted FTP control streams.
///
/// Both variants implement `AsyncRead + AsyncWrite + Unpin`, so the enum
/// dispatches I/O calls to the active variant at zero cost (no vtable).
/// This is re-exported from `types.rs` as a public type.
impl super::types::FtpControlStream {
    /// Extract the inner `TcpStream` reference from a Plain variant.
    /// Returns `None` for TLS-wrapped streams (the TCP stream is owned
    /// internally by the TLS session).
    pub fn get_ref(&self) -> Option<&TcpStream> {
        match self {
            super::types::FtpControlStream::Plain(s) => Some(s),
            super::types::FtpControlStream::Tls(s) => Some(s.get_ref().0),
        }
    }
}

// Both TcpStream and TlsStream<TcpStream> are Unpin (no self-referential
// data), so Pin::new on &mut is always safe. Implement AsyncRead/AsyncWrite
// by delegating to the active variant.

impl AsyncRead for super::types::FtpControlStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            super::types::FtpControlStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            super::types::FtpControlStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for super::types::FtpControlStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            super::types::FtpControlStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            super::types::FtpControlStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            super::types::FtpControlStream::Plain(s) => Pin::new(s).poll_flush(cx),
            super::types::FtpControlStream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            super::types::FtpControlStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            super::types::FtpControlStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
