//! HTTP CONNECT tunnel through a proxy for HTTPS downloads.

use tokio::net::TcpStream;
use tracing::info;

use crate::error::{Aria2Error, Result};
use crate::network::OutboundNetworkPolicy;

use super::config::HttpProxyConfig;
use super::io::{connect_to_proxy, connect_to_proxy_with_policy};

/// HTTP CONNECT tunnel through a proxy for HTTPS downloads.
///
/// The flow is:
/// 1. Connect to the proxy server
/// 2. Send CONNECT target_host:target_port HTTP/1.1\r\nHost: ...\r\n\r\n
/// 3. If 407 received, retry with Proxy-Authorization header
/// 4. If 200 received, the tunnel is established and the TcpStream is returned
///    for the caller to perform TLS handshake on
///
/// # Example
///
/// ``rust,ignore
/// use aria2_core::http::proxy::{HttpProxyConfig, HttpProxyTunnel};
///
/// let config = HttpProxyConfig::new(
///     "proxy.example.com".into(), 3128,
///     "target.example.com".into(), 443,
/// );
/// let tunnel = HttpProxyTunnel::new(config);
/// let stream = tunnel.connect().await?;
/// // Now perform TLS handshake on stream
/// ``
pub struct HttpProxyTunnel {
    config: HttpProxyConfig,
}

impl HttpProxyTunnel {
    /// Create a new CONNECT tunnel handler with the given configuration.
    pub fn new(config: HttpProxyConfig) -> Self {
        Self { config }
    }

    /// Establish an HTTP CONNECT tunnel through the proxy.
    ///
    /// On success, returns the TcpStream which is now tunneled -- bytes
    /// written to / read from it go directly to/from the target server.
    /// The caller should perform TLS handshake on this stream for HTTPS.
    pub async fn connect(&self) -> Result<TcpStream> {
        let stream = connect_to_proxy(&self.config).await?;
        self.finish_connect(stream).await
    }

    /// Establish the tunnel using the shared outbound source policy.
    pub async fn connect_with_policy(&self, policy: &OutboundNetworkPolicy) -> Result<TcpStream> {
        let stream = connect_to_proxy_with_policy(&self.config, policy).await?;
        self.finish_connect(stream).await
    }

    async fn finish_connect(&self, mut stream: TcpStream) -> Result<TcpStream> {
        let target = self.config.target_host_port();
        let options = aria2_protocol::http::connect::HttpConnectOptions {
            target_host: &self.config.target_host,
            target_port: self.config.target_port,
            user_agent: "",
            username: self.config.proxy_username.as_deref(),
            password: self.config.proxy_password.as_deref(),
            preemptive_basic_auth: false,
            write_timeout: self.config.write_timeout,
            read_timeout: self.config.read_timeout,
        };
        let retry_policy = aria2_protocol::http::connect::HttpConnectRetryPolicy {
            max_auth_retries: super::io::MAX_AUTH_RETRIES,
            prefer_digest: true,
            fallback_to_basic: true,
            proxy_connection_keep_alive: true,
        };
        aria2_protocol::http::connect::establish_connect_tunnel_with_retry_policy(
            &mut stream,
            &options,
            &retry_policy,
        )
        .await
        .map_err(|error| map_connect_error(error, &self.config))?;
        info!("CONNECT tunnel established to {} via proxy", target);
        Ok(stream)
    }
}

fn map_connect_error(
    error: aria2_protocol::http::connect::HttpConnectError,
    config: &HttpProxyConfig,
) -> Aria2Error {
    use aria2_protocol::http::connect::HttpConnectError;

    match error {
        HttpConnectError::Timeout => Aria2Error::Network("Timeout during proxy CONNECT".into()),
        HttpConnectError::InvalidRequestField(field) => {
            Aria2Error::InvalidArgument(format!("invalid HTTP CONNECT request field: {field}"))
        }
        HttpConnectError::Io(error) => {
            Aria2Error::Network(format!("Error communicating with proxy: {error}"))
        }
        HttpConnectError::InvalidResponse(message) => {
            Aria2Error::Parse(format!("Error parsing proxy response: {message}"))
        }
        HttpConnectError::CredentialsRequired => {
            Aria2Error::Network("Proxy requires authentication but no credentials provided".into())
        }
        HttpConnectError::UnsupportedAuthentication(_) => {
            Aria2Error::Network("Proxy requires auth but no supported scheme found".into())
        }
        HttpConnectError::AuthenticationFailed => {
            Aria2Error::Network("Proxy authentication failed after max retries".into())
        }
        HttpConnectError::Rejected {
            status_code,
            reason_phrase,
        } => Aria2Error::Network(format!(
            "Proxy returned error {status_code} {reason_phrase} for CONNECT to {}",
            config.target_host_port()
        )),
    }
}
