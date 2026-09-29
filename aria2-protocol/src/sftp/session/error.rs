use std::io;
use std::time::Duration;

use super::super::connection::SshError;

/// Errors produced while opening an SFTP session or exchanging packets.
#[derive(Debug, thiserror::Error)]
pub enum SftpSessionError {
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error("failed to encode SFTP packet: {0}")]
    PacketEncode(#[source] io::Error),
    #[error("failed to decode SFTP packet: {0}")]
    PacketDecode(#[source] io::Error),
    #[error("failed to write {operation}: {source}")]
    ChannelWrite {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to flush {operation}: {source}")]
    ChannelFlush {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to read SFTP channel: {0}")]
    ChannelRead(#[source] io::Error),
    #[error("timed out waiting for SFTP packet after {0:?}")]
    Timeout(Duration),
    #[error("SFTP channel closed by server")]
    ChannelClosed,
    #[error("server SFTP version {server_version} is below minimum {minimum}")]
    UnsupportedVersion { server_version: u32, minimum: u32 },
    #[error("expected SFTP {expected} packet, got packet type {actual_type}")]
    UnexpectedPacket {
        expected: &'static str,
        actual_type: u8,
    },
    #[error("SFTP response request ID mismatch: expected {expected}, got {actual:?}")]
    RequestIdMismatch { expected: u32, actual: Option<u32> },
}

impl SftpSessionError {
    /// Whether retrying the session operation may succeed after connectivity recovers.
    pub fn is_network(&self) -> bool {
        match self {
            Self::Ssh(error) => error.is_retryable(),
            Self::ChannelWrite { .. }
            | Self::ChannelFlush { .. }
            | Self::ChannelRead(_)
            | Self::Timeout(_)
            | Self::ChannelClosed => true,
            Self::PacketEncode(_)
            | Self::PacketDecode(_)
            | Self::UnsupportedVersion { .. }
            | Self::UnexpectedPacket { .. }
            | Self::RequestIdMismatch { .. } => false,
        }
    }
}
