//! UDP tracker URL syntax and the protocol's default announce port.

use thiserror::Error;
use url::Url;

/// Default UDP tracker port when the URL omits one.
pub const DEFAULT_UDP_TRACKER_PORT: u16 = 6881;

/// A validated UDP tracker endpoint for transport policy to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpTrackerUrl {
    host: String,
    port: u16,
}

impl UdpTrackerUrl {
    /// Parse the scheme, authority, host, and effective port of a UDP tracker URL.
    pub fn parse(value: &str) -> Result<Self, UdpTrackerUrlError> {
        let url =
            Url::parse(value).map_err(|error| UdpTrackerUrlError::InvalidUrl(error.to_string()))?;
        if url.scheme() != "udp" {
            return Err(UdpTrackerUrlError::UnsupportedScheme);
        }
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or(UdpTrackerUrlError::MissingHost)?;
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);

        Ok(Self {
            host: host.to_owned(),
            port: url.port().unwrap_or(DEFAULT_UDP_TRACKER_PORT),
        })
    }

    /// Return the normalized host that the caller should resolve.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Return the explicit port or [`DEFAULT_UDP_TRACKER_PORT`].
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Failure while parsing a UDP tracker URL.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UdpTrackerUrlError {
    /// The input could not be parsed as a URL.
    #[error("{0}")]
    InvalidUrl(String),
    /// The URL uses a scheme other than `udp`.
    #[error("unsupported tracker URL scheme")]
    UnsupportedScheme,
    /// The URL has no host component.
    #[error("tracker URL has no host")]
    MissingHost,
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_UDP_TRACKER_PORT, UdpTrackerUrl, UdpTrackerUrlError};

    #[test]
    fn parses_host_and_default_port() {
        let url = UdpTrackerUrl::parse("udp://tracker.example/announce").unwrap();

        assert_eq!(url.host(), "tracker.example");
        assert_eq!(url.port(), DEFAULT_UDP_TRACKER_PORT);
    }

    #[test]
    fn parses_explicit_ipv4_and_ipv6_endpoints() {
        let ipv4 = UdpTrackerUrl::parse("udp://192.0.2.1:6969/announce").unwrap();
        assert_eq!(ipv4.host(), "192.0.2.1");
        assert_eq!(ipv4.port(), 6969);

        let ipv6 = UdpTrackerUrl::parse("udp://[2001:db8::1]:6970/announce").unwrap();
        assert_eq!(ipv6.host(), "2001:db8::1");
        assert_eq!(ipv6.port(), 6970);
    }

    #[test]
    fn rejects_other_schemes_and_missing_hosts() {
        assert_eq!(
            UdpTrackerUrl::parse("https://tracker.example/announce"),
            Err(UdpTrackerUrlError::UnsupportedScheme)
        );
        assert_eq!(
            UdpTrackerUrl::parse("udp:///announce"),
            Err(UdpTrackerUrlError::MissingHost)
        );
    }
}
