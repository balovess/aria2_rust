//! FTP negotiation, proxy, and response helpers used by the download engine.

use std::net::SocketAddr;

mod negotiation;
mod proxy_get;
mod proxy_tunnel;

/// Bind an active-mode data listener to the control connection's interface.
///
/// The original FTP client creates the data socket from the control socket's
/// selected endpoint. Keeping this policy at the shared connection seam lets
/// the production engine and standalone negotiation adapter use one rule.
pub(crate) fn active_data_bind_addr(local_addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(local_addr.ip(), 0)
}

// Re-export negotiation types
pub use negotiation::{
    FtpDataProxyConfig, FtpMode, FtpNegotiationConfig, FtpNegotiationResult, FtpNegotiator,
    FtpTransferType, RawFtpControl, ServerCapabilities,
};
pub(crate) use negotiation::{
    cwd_targets, parse_epsv_response, parse_mdtm_timestamp, parse_pasv_response,
    parse_pwd_response, percent_decode, read_response_impl, split_decoded_remote_path,
};

// Re-export proxy tunnel types
pub use proxy_tunnel::{FtpProxyTunnel, FtpProxyTunnelConfig, FtpProxyTunnelResult};

// Re-export proxy GET types
pub use proxy_get::{
    FtpProxyConfig, FtpProxyGetRequest, FtpProxyGetRequestBuilder, FtpProxyGetResponse,
    ProxyMethod, execute_proxy_get, resolve_proxy_method,
};
