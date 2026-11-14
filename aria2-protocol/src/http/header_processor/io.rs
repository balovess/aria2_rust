//! Async response-header reads that preserve bytes after the header block.

use std::io;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::{HttpResponseHead, find_header_end, parse_response_head};

const MAX_RESPONSE_HEADER_BYTES: usize = 8 * 1024 + 4;

/// Failure while reading an HTTP response head from a TCP stream.
#[derive(Debug, Error)]
pub enum HttpResponseHeadReadError {
    /// The stream did not provide the next bytes before the deadline.
    #[error("timed out while reading HTTP response headers")]
    Timeout,
    /// The stream could not be read.
    #[error("HTTP response header I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The peer exceeded the configured response-header size bound.
    #[error("Too large HTTP header block")]
    HeaderTooLarge,
    /// The received header block was malformed.
    #[error("invalid HTTP response headers: {0}")]
    InvalidResponse(String),
}

/// Read and parse one response head without consuming bytes after its header
/// terminator. The timeout applies to each peek and corresponding read.
pub async fn read_response_head(
    stream: &mut TcpStream,
    read_timeout: Duration,
) -> Result<HttpResponseHead, HttpResponseHeadReadError> {
    let mut header = Vec::with_capacity(512);
    let mut peek_buffer = [0; MAX_RESPONSE_HEADER_BYTES];

    loop {
        let remaining = MAX_RESPONSE_HEADER_BYTES.saturating_sub(header.len());
        if remaining == 0 {
            return Err(HttpResponseHeadReadError::HeaderTooLarge);
        }
        let length = timeout(read_timeout, stream.peek(&mut peek_buffer[..remaining]))
            .await
            .map_err(|_| HttpResponseHeadReadError::Timeout)??;
        if length == 0 {
            return Err(HttpResponseHeadReadError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed while reading response headers",
            )));
        }

        let response_end = find_header_end(&peek_buffer[..length]).map(|index| index + 4);
        let bytes_to_consume = response_end.unwrap_or(length);
        let start = header.len();
        header.resize(start + bytes_to_consume, 0);
        timeout(read_timeout, stream.read_exact(&mut header[start..]))
            .await
            .map_err(|_| HttpResponseHeadReadError::Timeout)??;

        if response_end.is_some() {
            break;
        }
    }

    parse_response_head(&header)
        .map_err(|error| HttpResponseHeadReadError::InvalidResponse(error.to_string()))
}
