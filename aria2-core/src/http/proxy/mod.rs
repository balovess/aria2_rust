//! HTTP proxy support (CONNECT tunnel and forward proxy)
//!
//! Implements HTTP proxy functionality matching the C++ aria2
//! AbstractProxyRequestCommand, AbstractProxyResponseCommand,
//! HttpProxyRequestCommand, and HttpProxyResponseCommand.
//!
//! Two proxy modes are supported:
//!
//! - **CONNECT tunnel** ([HttpProxyTunnel]): For HTTPS downloads, sends
//!   CONNECT host:port HTTP/1.1 to the proxy, which establishes a blind
//!   TCP tunnel. The returned TcpStream can then be used for TLS.
//!
//! - **Forward proxy** ([HttpProxyForward]): For HTTP downloads, sends the
//!   request with the full URL (e.g., GET http://host:port/path HTTP/1.1).
//!   The proxy relays the request and response.
//!
//! Both modes support proxy authentication (407 Proxy Authentication Required)
//! via Basic and Digest schemes, reusing the existing [DigestAuthChallenge]
//! and [asic_auth] infrastructure.

pub mod auth;
pub mod config;
pub mod forward;
pub mod io;
pub mod response;
pub mod tunnel;

#[cfg(test)]
mod tests;

// Re-export key types for convenient access
pub use config::HttpProxyConfig;
pub use forward::{HttpProxyForward, forward_get_with_auth};
pub use response::ProxyResponse;
pub use tunnel::HttpProxyTunnel;
