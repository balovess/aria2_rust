//! TLS configuration for HTTPS RPC support.

use std::sync::Arc;

/// TLS configuration for HTTPS RPC server.
///
/// Contains paths to certificate and private key files in PEM format.
/// Used to enable TLS encryption for RPC communication.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Path to TLS certificate file (PEM format)
    pub cert_path: String,
    /// Path to TLS private key file (PEM format)
    pub key_path: String,
}

impl TlsConfig {
    /// Create a new TLS configuration with certificate and key paths.
    pub fn new(cert_path: impl Into<String>, key_path: impl Into<String>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
        }
    }

    /// Load and parse TLS configuration into a rustls ServerConfig.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Certificate file cannot be read or parsed
    /// - Private key file cannot be read or parsed
    /// - Certificate/key combination is invalid
    pub fn load_server_config(&self) -> Result<Arc<rustls::ServerConfig>, TlsError> {
        use rustls_pemfile::{certs, private_key};
        use std::io::BufReader;

        // Read certificate file
        let cert_file = std::fs::File::open(&self.cert_path)
            .map_err(|e| TlsError::CertificateRead(self.cert_path.clone(), e))?;
        let mut cert_reader = BufReader::new(cert_file);
        let cert_chain: Vec<rustls::pki_types::CertificateDer> = certs(&mut cert_reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(TlsError::CertificateParse)?;

        if cert_chain.is_empty() {
            return Err(TlsError::NoCertificates);
        }

        // Read private key file
        let key_file = std::fs::File::open(&self.key_path)
            .map_err(|e| TlsError::KeyRead(self.key_path.clone(), e))?;
        let mut key_reader = BufReader::new(key_file);
        let key = private_key(&mut key_reader)
            .map_err(TlsError::KeyParse)?
            .ok_or(TlsError::NoPrivateKey)?;

        // Build server config
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(TlsError::InvalidConfig)?;

        Ok(Arc::new(config))
    }
}

/// Errors that can occur during TLS configuration loading.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("Failed to read certificate file '{0}': {1}")]
    CertificateRead(String, std::io::Error),
    #[error("Failed to parse certificates: {0}")]
    CertificateParse(std::io::Error),
    #[error("No certificates found in certificate file")]
    NoCertificates,
    #[error("Failed to read private key file '{0}': {1}")]
    KeyRead(String, std::io::Error),
    #[error("Failed to parse private key: {0}")]
    KeyParse(std::io::Error),
    #[error("No private key found in key file")]
    NoPrivateKey,
    #[error("Invalid TLS configuration: {0}")]
    InvalidConfig(rustls::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_config_new() {
        let tls = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem");
        assert_eq!(tls.cert_path, "/path/to/cert.pem");
        assert_eq!(tls.key_path, "/path/to/key.pem");
    }

    #[test]
    fn test_tls_error_display() {
        let err = TlsError::NoCertificates;
        assert!(err.to_string().contains("No certificates"));

        let err = TlsError::NoPrivateKey;
        assert!(err.to_string().contains("No private key"));
    }
}
