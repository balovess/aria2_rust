//! HTTP forward proxy for non-HTTPS downloads.
//!
//! Also provides the [orward_get_with_auth] convenience function that
//! combines proxy connection, request sending, and auth handling.

use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, Result};
use crate::http::header_processor::HttpResponseHead;

use super::auth::build_proxy_auth_header;
use super::config::HttpProxyConfig;
use super::io::{MAX_AUTH_RETRIES, connect_to_proxy, read_proxy_response, write_all_timeout};
use super::response::ProxyResponse;

/// HTTP forward proxy for non-HTTPS downloads.
///
/// In forward mode, the proxy acts as a relay. The client sends requests with
/// the full URL (e.g., GET http://target:port/path HTTP/1.1) instead of just
/// the path. The proxy forwards the request to the target and relays the response.
///
/// For proxy authentication (407), the Proxy-Authorization header is added
/// on retry.
///
/// # Example
///
/// ``rust,ignore
/// use aria2_core::http::proxy::{HttpProxyConfig, HttpProxyForward};
///
/// let config = HttpProxyConfig::new(
///     "proxy.example.com".into(), 3128,
///     "target.example.com".into(), 80,
/// );
/// let forward = HttpProxyForward::new(config);
/// // Send the initial request and handle 407 retry
/// let stream = forward.connect().await?;
/// // Now send the actual HTTP request with full URL through stream
/// ``
pub struct HttpProxyForward {
    config: HttpProxyConfig,
}

impl HttpProxyForward {
    /// Create a new forward proxy handler with the given configuration.
    pub fn new(config: HttpProxyConfig) -> Self {
        Self { config }
    }

    /// Connect to the proxy and verify it is reachable.
    ///
    /// For forward proxy, we simply establish the TCP connection to the proxy.
    /// The actual HTTP request with the full URL is sent by the caller on the
    /// returned stream. This method also performs a lightweight handshake check:
    /// it sends an HTTP HEAD request and handles 407 if needed, then returns
    /// the stream ready for the actual request.
    ///
    /// If skip_handshake is true, only the TCP connection is established
    /// without any probe request. This is useful when the caller will
    /// immediately send their own request.
    pub async fn connect(&self, skip_handshake: bool) -> Result<TcpStream> {
        let mut stream = connect_to_proxy(&self.config).await?;

        if skip_handshake {
            return Ok(stream);
        }

        // Send a probe HEAD request to check if proxy auth is needed
        let target_url = format!(
            "http://{}:{}",
            self.config.target_host, self.config.target_port
        );
        let probe_request = self.build_forward_request("HEAD", &target_url, "/", None);
        debug!("Sending probe HEAD request to proxy for {}", target_url);
        write_all_timeout(
            &mut stream,
            probe_request.as_bytes(),
            self.config.write_timeout,
        )
        .await?;

        let mut auth_nc = 1u32;

        loop {
            let head = read_proxy_response(&mut stream, self.config.read_timeout).await?;
            let proxy_resp = ProxyResponse::from_head(head);

            match proxy_resp {
                ProxyResponse::Connected(_) => {
                    info!("Forward proxy connection ready for {}", target_url);
                    // For a HEAD probe, a 200 means we're good.
                    // But the stream has consumed the response; we need a fresh
                    // connection for the actual data request.
                    drop(stream);
                    return connect_to_proxy(&self.config).await;
                }
                ProxyResponse::AuthRequired { response } => {
                    let (username, password) = self.get_credentials()?;

                    if auth_nc > MAX_AUTH_RETRIES {
                        return Err(Aria2Error::Network(
                            "Proxy authentication failed after max retries".to_string(),
                        ));
                    }

                    let auth_value = build_proxy_auth_header(
                        &response,
                        &username,
                        &password,
                        "HEAD",
                        &target_url,
                        auth_nc,
                    )
                    .ok_or_else(|| {
                        Aria2Error::Network(
                            "Proxy requires auth but no supported scheme found".to_string(),
                        )
                    })?;

                    auth_nc += 1;
                    warn!(
                        "Proxy returned 407, retrying with authentication (attempt {})",
                        auth_nc
                    );

                    // Close this connection and open a new one for the retry
                    drop(stream);
                    stream = connect_to_proxy(&self.config).await?;

                    let retry_request =
                        self.build_forward_request("HEAD", &target_url, "/", Some(&auth_value));
                    write_all_timeout(
                        &mut stream,
                        retry_request.as_bytes(),
                        self.config.write_timeout,
                    )
                    .await?;
                }
                ProxyResponse::Error {
                    status_code,
                    reason,
                } => {
                    // Some proxies return 403 or other errors for HEAD probes
                    // but work fine for actual GET requests. Log a warning
                    // and return the stream anyway so the caller can try.
                    warn!(
                        "Proxy returned {} {} on probe, returning stream for caller to retry",
                        status_code, reason
                    );
                    drop(stream);
                    return connect_to_proxy(&self.config).await;
                }
            }
        }
    }

    /// Build a forward proxy request with the full URL.
    ///
    /// In forward proxy mode, the request line uses the absolute URL:
    /// METHOD http://host:port/path HTTP/1.1
    ///
    /// # Arguments
    /// * method - HTTP method (GET, HEAD, etc.)
    /// * ull_url - The full URL including scheme and host (e.g., http://target:80/path)
    /// * path - The path component (used for Digest auth URI)
    /// * proxy_auth - Optional Proxy-Authorization header value
    pub fn build_forward_request(
        &self,
        method: &str,
        full_url: &str,
        path: &str,
        proxy_auth: Option<&str>,
    ) -> String {
        let mut req = format!("{} {} HTTP/1.1\r\n", method, full_url);
        req.push_str(&format!("Host: {}\r\n", self.config.target_host_port()));

        if self.config.target_port != 80 {
            // Already included in Host above if non-standard
        }

        if let Some(auth) = proxy_auth {
            req.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
        }

        req.push_str("User-Agent: aria2-rust/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n");

        let _ = path; // Path is used by the caller for Digest auth URI computation
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

// ---------------------------------------------------------------------------
// Free helper: send a request through a forward proxy and handle 407
// ---------------------------------------------------------------------------

/// Send an HTTP GET request through a forward proxy, handling 407 auth retry.
///
/// This is a convenience function that combines proxy connection, request
/// sending, and auth handling into a single call. On success, returns the
/// TcpStream positioned after the response headers (ready to read body),
/// along with the parsed [HttpResponseHead].
///
/// # Arguments
/// * config - Proxy configuration
/// * path - The request path on the target (e.g., /download/file.zip)
///
/// # Returns
/// A tuple of (TcpStream, HttpResponseHead) where the stream is ready
/// for reading the response body.
pub async fn forward_get_with_auth(
    config: &HttpProxyConfig,
    path: &str,
) -> Result<(TcpStream, HttpResponseHead)> {
    let full_url = format!(
        "http://{}:{}{}",
        config.target_host, config.target_port, path
    );

    let mut stream = connect_to_proxy(config).await?;
    let mut auth_nc = 1u32;
    let mut current_auth: Option<String> = None;

    loop {
        let request = if let Some(ref auth) = current_auth {
            format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: {}\r\nUser-Agent: aria2-rust/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
                full_url,
                config.target_host_port(),
                auth
            )
        } else {
            format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: aria2-rust/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
                full_url,
                config.target_host_port()
            )
        };

        write_all_timeout(&mut stream, request.as_bytes(), config.write_timeout).await?;
        let head = read_proxy_response(&mut stream, config.read_timeout).await?;
        let proxy_resp = ProxyResponse::from_head(head);

        match proxy_resp {
            ProxyResponse::Connected(head) => {
                return Ok((stream, head));
            }
            ProxyResponse::AuthRequired { response } => {
                let username = config.proxy_username.as_deref().unwrap_or("");
                let password = config.proxy_password.as_deref().unwrap_or("");

                if username.is_empty() && config.proxy_username.is_none() {
                    return Err(Aria2Error::Network(
                        "Proxy requires authentication but no credentials provided".to_string(),
                    ));
                }

                if auth_nc > MAX_AUTH_RETRIES {
                    return Err(Aria2Error::Network(
                        "Proxy authentication failed after max retries".to_string(),
                    ));
                }

                let auth_value = build_proxy_auth_header(
                    &response, username, password, "GET", &full_url, auth_nc,
                )
                .ok_or_else(|| {
                    Aria2Error::Network(
                        "Proxy requires auth but no supported scheme found".to_string(),
                    )
                })?;

                auth_nc += 1;
                current_auth = Some(auth_value);

                warn!(
                    "Proxy returned 407, retrying GET with auth (attempt {})",
                    auth_nc
                );

                // Open a new connection for the retry
                drop(stream);
                stream = connect_to_proxy(config).await?;
            }
            ProxyResponse::Error {
                status_code,
                reason,
            } => {
                return Err(Aria2Error::Network(format!(
                    "Proxy returned error {} {} for GET {}",
                    status_code, reason, full_url
                )));
            }
        }
    }
}
