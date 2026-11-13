//! CONNECT tunnel establishment and I/O helpers

use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::error::{Aria2Error, RecoverableError, Result};

use super::HttpProxyTunnelConfig;

// ---------------------------------------------------------------------------
// HttpProxyTunnel -- internal implementation
// ---------------------------------------------------------------------------

/// HTTP proxy tunnel handler.
pub(crate) struct HttpProxyTunnel;

impl HttpProxyTunnel {
    pub(crate) async fn establish_tunnel(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        let stream = Self::connect_to_proxy(config).await?;
        let stream = Self::establish_tunnel_on_stream(stream, config, true).await?;
        info!(
            "HTTP CONNECT tunnel established: {}:{} -> {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );
        Ok(stream)
    }

    /// Establish a CONNECT tunnel over a stream opened by the caller.
    ///
    /// The caller retains ownership of DNS, socket binding, and outbound
    /// network policy. `preemptive_basic_auth` controls whether configured
    /// credentials are sent before the proxy issues a challenge.
    pub(crate) async fn establish_tunnel_on_stream(
        mut stream: TcpStream,
        config: &HttpProxyTunnelConfig,
        preemptive_basic_auth: bool,
    ) -> Result<TcpStream> {
        Self::set_nodelay(&stream)?;
        let options = aria2_protocol::http::connect::HttpConnectOptions {
            target_host: &config.target_host,
            target_port: config.target_port,
            user_agent: &config.user_agent,
            username: config.username.as_deref(),
            password: config.password.as_deref(),
            preemptive_basic_auth,
            write_timeout: config.write_timeout,
            read_timeout: config.read_timeout,
        };
        aria2_protocol::http::connect::establish_connect_tunnel(&mut stream, &options)
            .await
            .map_err(|error| map_connect_error(error, config))?;
        Ok(stream)
    }

    pub(crate) async fn establish_forward(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        let stream = Self::connect_to_proxy(config).await?;
        Self::set_nodelay(&stream)?;
        info!(
            "HTTP forward-proxy connection: {}:{} for target {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );
        Ok(stream)
    }

    // -- Connection & handshake ------------------------------------------------

    async fn connect_to_proxy(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        debug!(
            "Connecting to proxy {}:{} for {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );
        let stream = timeout(
            config.connect_timeout,
            TcpStream::connect((config.proxy_host.as_str(), config.proxy_port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!(
                    "Failed to connect to proxy {}:{}: {}",
                    config.proxy_host, config.proxy_port, e
                ),
            })
        })?;
        Ok(stream)
    }

    fn set_nodelay(stream: &TcpStream) -> Result<()> {
        stream.set_nodelay(true).map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {error}"),
            })
        })
    }
}

fn map_connect_error(
    error: aria2_protocol::http::connect::HttpConnectError,
    config: &HttpProxyTunnelConfig,
) -> Aria2Error {
    use aria2_protocol::http::connect::HttpConnectError;

    match error {
        HttpConnectError::Timeout => Aria2Error::Recoverable(RecoverableError::Timeout),
        HttpConnectError::InvalidResponse(message) => Aria2Error::Parse(message),
        HttpConnectError::Io(error) => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP CONNECT I/O failed: {error}"),
            })
        }
        HttpConnectError::CredentialsRequired => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!(
                    "Proxy {}:{} requires auth but no credentials provided",
                    config.proxy_host, config.proxy_port
                ),
            })
        }
        HttpConnectError::UnsupportedAuthentication(challenge) => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Unsupported proxy auth: {challenge}"),
            })
        }
        HttpConnectError::AuthenticationFailed => {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!(
                    "Proxy auth failed for {}:{}",
                    config.proxy_host, config.proxy_port
                ),
            })
        }
        HttpConnectError::Rejected {
            status_code,
            reason_phrase,
        } => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!(
                "Proxy {}:{} rejected CONNECT to {}:{}: {} {}",
                config.proxy_host,
                config.proxy_port,
                config.target_host,
                config.target_port,
                status_code,
                reason_phrase
            ),
        }),
    }
}
