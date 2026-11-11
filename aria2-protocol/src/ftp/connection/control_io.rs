//! Transport-independent FTP control-channel I/O primitives.
//!
//! Callers establish and configure the stream, then pass its reader or writer
//! here. This lets the standalone client use a direct TCP stream while the
//! engine injects address-bound, proxied, or TLS-protected streams.

use std::time::Duration;
use std::{fmt, io};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

pub(super) const MAX_RESPONSE_BYTES: usize = 65_536;

#[derive(Debug)]
pub(super) enum BoundedControlReadError {
    Timeout,
    Io(io::Error),
    TooLarge(usize),
}

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

/// Read a line without buffering more than `max_bytes` from the stream.
///
/// Unlike `read_control_line`, this helper bounds memory before appending
/// input. FTP response parsers use it to enforce the protocol response limit
/// even when a peer sends a single unterminated line.
pub(super) async fn read_control_line_bounded<R>(
    reader: &mut R,
    line: &mut String,
    timeout_duration: Duration,
    max_bytes: usize,
) -> Result<usize, BoundedControlReadError>
where
    R: AsyncBufRead + Unpin,
{
    match timeout(timeout_duration, async {
        let mut bytes = Vec::with_capacity(max_bytes.min(4096));
        loop {
            let available = reader
                .fill_buf()
                .await
                .map_err(BoundedControlReadError::Io)?;
            if available.is_empty() {
                break;
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let bytes_to_consume = newline.map_or(available.len(), |index| index + 1);
            if bytes_to_consume > max_bytes.saturating_sub(bytes.len()) {
                return Err(BoundedControlReadError::TooLarge(
                    max_bytes.saturating_add(1),
                ));
            }

            bytes.extend_from_slice(&available[..bytes_to_consume]);
            reader.consume(bytes_to_consume);
            if newline.is_some() {
                break;
            }
        }

        let bytes_read = bytes.len();
        let text = String::from_utf8(bytes).map_err(|error| {
            BoundedControlReadError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;
        line.push_str(&text);
        Ok(bytes_read)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(BoundedControlReadError::Timeout),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn bounded_line_reader_rejects_oversized_line_before_appending() {
        let input = vec![b'x'; 16];
        let mut reader = BufReader::new(std::io::Cursor::new(input));
        let mut line = String::new();

        assert!(matches!(
            read_control_line_bounded(&mut reader, &mut line, Duration::from_secs(1), 8).await,
            Err(BoundedControlReadError::TooLarge(9))
        ));
        assert!(line.is_empty());
    }
}
