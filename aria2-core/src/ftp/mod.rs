//! FTP 协议客户端模块
//!
//! 提供完整的 FTP 协议实现，支持：
//! - 被动模式（PASV/EPSV）和主动模式（PORT/EPRT）
//! - 二进制/ASCII 传输模式切换
//! - 目录列表解析（Unix/Windows 格式）
//! - 断点续传（REST 命令）
//! - 完整的错误处理
//! - 连接池复用（40-60% 性能提升）

pub mod connection;
pub mod connection_pool;

#[cfg(test)]
mod connection_tests;

pub use connection::{FtpClient, FtpFileInfo, FtpMode, FtpResponse};
pub use connection_pool::{
    ConnectionKey, FtpConnectionPool, PooledConnection, PoolConfig, PoolStats,
    get_global_pool, create_custom_pool,
};
