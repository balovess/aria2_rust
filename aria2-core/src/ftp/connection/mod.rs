//! FTP negotiation, proxy, and response helpers used by the download engine.

use std::net::SocketAddr;

mod parsing;
mod proxy_get;
mod proxy_tunnel;
mod response;

/// FTP data connection mode used by the connection pool and download options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtpMode {
    /// Passive mode (the client connects to the server's data port).
    #[default]
    Passive,
    /// Active mode (the server accepts a connection from the server).
    Active,
}

/// Bind an active-mode data listener to the control connection's interface.
///
/// The download engine creates the data socket from the control socket's
/// selected endpoint. Keeping this policy beside the connection primitives
/// gives active-mode setup one owner.
pub(crate) fn active_data_bind_addr(local_addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(local_addr.ip(), 0)
}

// Re-export parsing helpers used by the download engine.
pub(crate) use parsing::{
    cwd_targets, parse_epsv_response, parse_mdtm_timestamp, parse_pasv_response,
    parse_pwd_response, percent_decode, split_decoded_remote_path,
};
pub(crate) use response::read_response_impl;

// Re-export proxy tunnel types
pub use proxy_tunnel::{FtpProxyTunnel, FtpProxyTunnelConfig, FtpProxyTunnelResult};

// Re-export proxy GET types
pub use proxy_get::{
    FtpProxyConfig, FtpProxyGetRequest, FtpProxyGetRequestBuilder, FtpProxyGetResponse,
    ProxyMethod, execute_proxy_get, resolve_proxy_method,
};
