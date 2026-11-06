//! Rust-native HTTPS client TLS configuration.

use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadOptions;

mod pkcs12;

#[cfg(test)]
mod pkcs12_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

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
    min_tls_version: Option<String>,
}

impl Default for ClientTlsConfig {
    fn default() -> Self {
        Self {
            check_certificate: true,
            ca_certificate: None,
            certificate: None,
            private_key: None,
            min_tls_version: None,
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
            min_tls_version: options.min_tls_version.clone(),
        }
    }

    pub(crate) fn requires_custom_client(&self) -> bool {
        !self.check_certificate
            || self.ca_certificate.is_some()
            || self.certificate.is_some()
            || self.private_key.is_some()
            || self.min_tls_version.is_some()
    }

    fn minimum_version(&self) -> Result<Option<reqwest::tls::Version>> {
        self.min_tls_version
            .as_deref()
            .map(|version| match version {
                "TLSv1.1" => Ok(reqwest::tls::Version::TLS_1_1),
                "TLSv1.2" => Ok(reqwest::tls::Version::TLS_1_2),
                "TLSv1.3" => Ok(reqwest::tls::Version::TLS_1_3),
                _ => Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("Unsupported minimum TLS version '{version}'"),
                ))),
            })
            .transpose()
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
    let builder = if let Some(version) = config.minimum_version()? {
        builder.tls_version_min(version)
    } else {
        builder
    };
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
            let identity = pkcs12::load_empty_password_identity(certificate)?;
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
