//! FTP protocol client module.
//!
//! The engine's FTP path is built from the connection streams and negotiation
//! primitives in [`connection`]. [`connection::FtpClient`] remains a legacy
//! standalone adapter for external library users; it is not injected into the
//! download engine.
//!
//! The module provides support for:
//! - Passive mode (PASV/EPSV) and active mode (PORT/EPRT)
//! - Binary/ASCII transfer mode switching
//! - Directory listing parsing (Unix/Windows formats)
//! - Resume/restart transfers (REST command)
//! - Comprehensive error handling
//! - Optional standalone connection-pool support for callers that can safely
//!   reuse plain FTP control streams
//! - Post-SIZE file reconciliation and resume handling

pub mod connection;
pub mod connection_pool;

#[cfg(test)]
mod connection_tests;

pub use connection::{
    FtpClient, FtpDataProxyConfig, FtpDataStream, FtpFileInfo, FtpMode, FtpProxyConfig,
    FtpProxyGetRequest, FtpProxyGetRequestBuilder, FtpResponse, FtpTransferType, ProxyMethod,
    resolve_proxy_method,
};
pub use connection_pool::{
    ConnectionKey, FtpConnectionPool, PoolConfig, PoolStats, PooledConnection, create_custom_pool,
    create_pool,
};
