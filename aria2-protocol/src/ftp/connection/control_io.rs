//! Transport-independent FTP control-channel I/O primitives.
//!
//! Callers establish and configure the stream, then pass its reader or writer
//! here. This lets the standalone client use a direct TCP stream while the
//! engine injects address-bound, proxied, or TLS-protected streams.

use std::time::Duration;
use std::{fmt, io};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

#[derive(Debug)]
pub enum FtpControlReadError {
    Timeout,
    Io(io::Error),
}

impl fmt::Display for FtpControlReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("FTP control response timed out"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FtpControlReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Timeout => None,
            Self::Io(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub enum FtpControlWriteError {
    Command(io::Error),
    Terminator(io::Error),
    Flush(io::Error),
}

impl fmt::Display for FtpControlWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(error) | Self::Terminator(error) | Self::Flush(error) => {
                error.fmt(formatter)
            }
        }
    }
}

impl std::error::Error for FtpControlWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command(error) | Self::Terminator(error) | Self::Flush(error) => Some(error),
        }
    }
}

/// Read one FTP control line under the caller's timeout policy.
pub async fn read_control_line<R>(
    reader: &mut R,
    line: &mut String,
    timeout_duration: Duration,
) -> Result<usize, FtpControlReadError>
where
    R: AsyncBufRead + Unpin,
{
    match timeout(timeout_duration, reader.read_line(line)).await {
        Ok(Ok(bytes_read)) => Ok(bytes_read),
        Ok(Err(error)) => Err(FtpControlReadError::Io(error)),
        Err(_) => Err(FtpControlReadError::Timeout),
    }
}

/// Write one FTP command and its CRLF terminator to an injected stream.
pub async fn write_control_command<W>(
    writer: &mut W,
    command: &str,
) -> Result<(), FtpControlWriteError>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(command.as_bytes())
        .await
        .map_err(FtpControlWriteError::Command)?;
    writer
        .write_all(b"\r\n")
        .await
        .map_err(FtpControlWriteError::Terminator)?;
    writer.flush().await.map_err(FtpControlWriteError::Flush)
}
