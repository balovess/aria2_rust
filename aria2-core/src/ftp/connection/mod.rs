//! FTP connection primitives and standalone adapters.
//!
//! The production download engine uses [`FtpControlStream`], [`FtpDataStream`],
//! and the negotiation API. [`FtpClient`] is retained as a standalone legacy
//! adapter for library users and tests; it is not the engine's download path.
//!
//! `FtpClient` exposes passive and active helpers that return plain
//! [`std::net::TcpStream`] values. Consequently, callers requiring an FTPS
//! protected data channel must use [`FtpNegotiator`] instead of treating those
//! helpers as a complete RFC 4217 data-channel implementation.

use std::net::SocketAddr;

mod commands;
mod connector;
mod feat;
mod negotiation;
mod parser;
mod proxy_get;
mod proxy_tunnel;
mod transfer;
mod types;

/// Bind an active-mode data listener to the control connection's interface.
///
/// The original FTP client creates the data socket from the control socket's
/// selected endpoint. Keeping this policy at the shared connection seam lets
/// the production engine and standalone negotiation adapter use one rule.
pub(crate) fn active_data_bind_addr(local_addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(local_addr.ip(), 0)
}

#[cfg(test)]
mod tests;

// Re-export all public types to preserve the original API
pub use types::{FtpClient, FtpFeatures, FtpFileInfo, FtpMode, FtpResponse, FtpTlsMode};

// Keep the historical core paths as thin compatibility re-exports. The TLS
// implementation and stream types live in aria2-protocol::ftp::tls.
pub use aria2_protocol::ftp::tls::{
    FtpControlStream, FtpDataStream, FtpsConfig, TlsVersion, build_tls_connector,
    perform_tls_handshake, upgrade_control_stream, upgrade_data_stream,
};

// Re-export negotiation types
pub use negotiation::{
    FtpDataProxyConfig, FtpNegotiationConfig, FtpNegotiationResult, FtpNegotiator, FtpTransferType,
    RawFtpControl, ServerCapabilities,
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
