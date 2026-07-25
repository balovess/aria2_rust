//! HTTP CONNECT tunnel through a proxy for HTTPS downloads.

use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, Result};

use super::auth::build_proxy_auth_header;
use super::config::HttpProxyConfig;
use super::io::{connect_to_proxy, read_proxy_response, write_all_timeout, MAX_AUTH_RETRIES};
use super::response::ProxyResponse;

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
        let mut stream = connect_to_proxy(&self.config).await?;

        let target = self.config.target_host_port();
        let mut auth_nc = 1u32;

        // Initial CONNECT request (no auth)
        let request = self.build_connect_request(None);
        debug!("Sending CONNECT request to proxy for {}", target);
        write_all_timeout(&mut stream, request.as_bytes(), self.config.write_timeout).await?;

        loop {
            let head = read_proxy_response(&mut stream, self.config.read_timeout).await?;
            let proxy_resp = ProxyResponse::from_head(head);

            match proxy_resp {
                ProxyResponse::Connected(head) => {
                    info!(
                        "CONNECT tunnel established to {} via proxy",
                        target
                    );
                    debug!("Proxy response: {:?}", head);
                    return Ok(stream);
                }
                ProxyResponse::AuthRequired { response } => {
                    let (username, password) = self.get_credentials()?;

                    if auth_nc > MAX_AUTH_RETRIES {
                        return Err(Aria2Error::Network(
                            "Proxy authentication failed after max retries".to_string(),
                        ));
                    }

                    // Build Proxy-Authorization header for the CONNECT method
                    let auth_value = build_proxy_auth_header(
                        &response,
                        &username,
                        &password,
                        "CONNECT",
                        &target,
                        auth_nc,
                    ).ok_or_else(|| {
                        Aria2Error::Network(
                            "Proxy requires auth but no supported scheme found".to_string(),
                        )
                    })?;

                    auth_nc += 1;
                    warn!("Proxy returned 407, retrying CONNECT with authentication (attempt {})", auth_nc);

                    // Re-send CONNECT with Proxy-Authorization
                    let request = self.build_connect_request(Some(&auth_value));
                    write_all_timeout(&mut stream, request.as_bytes(), self.config.write_timeout).await?;
                }
                ProxyResponse::Error { status_code, reason } => {
                    return Err(Aria2Error::Network(format!(
                        "Proxy returned error {} {} for CONNECT to {}",
                        status_code, reason, target
                    )));
                }
            }
        }
    }

    /// Build the CONNECT request string.
    pub(crate) fn build_connect_request(&self, proxy_auth: Option<&str>) -> String {
        let target = self.config.target_host_port();
        let mut req = format!(
            "CONNECT {} HTTP/1.1\r\nHost: {}\r\n",
            target, target
        );

        if let Some(auth) = proxy_auth {
            req.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
        }

        // Proxy-Connection: keep-alive is standard for CONNECT tunnels
        req.push_str("Proxy-Connection: keep-alive\r\n");
        req.push_str("\r\n");
        req
    }

    /// Extract credentials or return an error.
    pub(crate) fn get_credentials(&self) -> Result<(String, String)> {
        match (&self.config.proxy_username, &self.config.proxy_password) {
            (Some(u), Some(p)) => Ok((u.clone(), p.clone())),
            (Some(u), None) => Ok((u.clone(), String::new())),
            _ => Err(Aria2Error::Network(
                "Proxy requires authentication but no credentials provided".to_string(),
            )),
        }
    }
}
