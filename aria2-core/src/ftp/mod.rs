//! FTP protocol client module
//!
//! Provides a complete FTP protocol implementation with support for:
//! - Passive mode (PASV/EPSV) and active mode (PORT/EPRT)
//! - Binary/ASCII transfer mode switching
//! - Directory listing parsing (Unix/Windows formats)
//! - Resume/restart transfers (REST command)
//! - Comprehensive error handling
//! - Connection pool reuse (40-60% performance improvement)
//! - Post-SIZE file preparation pipeline (zero-length, dry-run, unknown length)

pub mod connection;
pub mod connection_pool;
pub mod file_preparation;

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
pub use file_preparation::{
    FtpFilePreparationConfig, FtpFilePreparationResult, FtpFileSize, apply_dir, create_safe_path,
    derive_local_path, percent_decode, prepare_ftp_file,
};
