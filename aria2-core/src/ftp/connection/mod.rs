//! FTP protocol client implementation
//!
//! Provides an async FTP client supporting passive/active mode, binary transfer,
//! directory listing parsing, and more.

mod commands;
mod connector;
mod feat;
mod ftp_finish;
mod negotiation;
mod parser;
mod proxy_get;
mod proxy_tunnel;
mod transfer;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types to preserve the original API
pub use types::{FtpClient, FtpFeatures, FtpFileInfo, FtpMode, FtpResponse};

// Re-export negotiation types
pub use negotiation::{
    FtpDataProxyConfig, FtpNegotiationConfig, FtpNegotiationResult, FtpNegotiator, FtpTransferType,
    RawFtpControl, ServerCapabilities,
};

// Re-export finish handler types
pub use ftp_finish::{FtpFinishConfig, FtpFinishHandler, FtpFinishResult, PooledFtpControl};

// Re-export proxy tunnel types
pub use proxy_tunnel::{FtpProxyTunnel, FtpProxyTunnelConfig, FtpProxyTunnelResult};

// Re-export proxy GET types
pub use proxy_get::{
    FtpProxyConfig, FtpProxyGetRequest, FtpProxyGetRequestBuilder, ProxyMethod,
    resolve_proxy_method,
};
