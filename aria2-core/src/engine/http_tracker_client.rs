//! HTTP/HTTPS tracker transport helpers.
//!
//! The announce lifecycle is owned by `bt_tracker_comm`; this module only
//! provides HTTP client construction used by that implementation.

use std::net::IpAddr;
use std::time::Duration;

use crate::http::client_identity::ClientTlsConfig;

/// Build a tracker client with independent request and TCP connection deadlines.
#[allow(dead_code)]
pub(crate) fn build_tracker_client_with_tls_and_timeouts(
    request_timeout_secs: u64,
    connect_timeout_secs: u64,
    tls: &ClientTlsConfig,
) -> Result<reqwest::Client, String> {
    build_tracker_client_with_source(request_timeout_secs, connect_timeout_secs, tls, None)
}

/// Build a tracker client pinned to one local source address.
pub(crate) fn build_tracker_client_with_source(
    request_timeout_secs: u64,
    connect_timeout_secs: u64,
    tls: &ClientTlsConfig,
    local_address: Option<IpAddr>,
) -> Result<reqwest::Client, String> {
    crate::http::client_pool::ensure_rustls_provider();
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(request_timeout_secs))
        .connect_timeout(Duration::from_secs(connect_timeout_secs))
        // aria2 does not advertise compressed tracker responses by default.
        // Keep this independent client aligned with the download clients.
        .gzip(false);
    if let Some(address) = local_address {
        builder = builder.local_address(address);
    }
    let builder =
        crate::http::client_identity::apply(builder, tls).map_err(|error| error.to_string())?;
    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracker_client_applies_custom_tls_configuration() {
        let options = crate::request::request_group::DownloadOptions {
            ca_certificate: Some("missing-tracker-ca.pem".into()),
            ..Default::default()
        };
        let tls = ClientTlsConfig::from_download_options(&options);
        let client = build_tracker_client_with_tls_and_timeouts(30, 30, &tls);
        assert!(
            client
                .expect_err("missing tracker CA must reject client construction")
                .contains("Failed to read CA certificate")
        );
    }
}
