//! FTP protocol client implementation
//!
//! Provides an async FTP client supporting passive/active mode, binary transfer,
//! directory listing parsing, FTPS (FTP over TLS per RFC 4217), and more.

use std::net::SocketAddr;

mod commands;
mod connector;
mod feat;
mod ftp_finish;
mod negotiation;
mod parser;
mod proxy_get;
mod proxy_tunnel;
mod tls;
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
pub use types::{
    FtpClient, FtpControlStream, FtpDataStream, FtpFeatures, FtpFileInfo, FtpMode, FtpResponse,
    FtpTlsMode, FtpsConfig, TlsVersion,
};

// Re-export FTPS TLS functions
pub use tls::{
    build_tls_connector, perform_tls_handshake, upgrade_control_stream, upgrade_data_stream,
};

// Re-export negotiation types
pub use negotiation::{
    FtpDataProxyConfig, FtpNegotiationConfig, FtpNegotiationResult, FtpNegotiator, FtpTransferType,
    RawFtpControl, ServerCapabilities,
};
pub(crate) use negotiation::{
    cwd_targets, parse_mdtm_timestamp, parse_pwd_response, percent_decode, read_response_impl,
    split_decoded_remote_path,
};

// Re-export finish handler types
pub use ftp_finish::{FtpFinishConfig, FtpFinishHandler, FtpFinishResult, PooledFtpControl};

// Re-export proxy tunnel types
pub use proxy_tunnel::{FtpProxyTunnel, FtpProxyTunnelConfig, FtpProxyTunnelResult};

// Re-export proxy GET types
pub use proxy_get::{
    FtpProxyConfig, FtpProxyGetRequest, FtpProxyGetRequestBuilder, FtpProxyGetResponse,
    ProxyMethod, execute_proxy_get, resolve_proxy_method,
};
