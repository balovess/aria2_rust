//! Incremental HTTP/1.x response-header parsing for protocol transports.

mod error;
mod processor;
mod types;

pub use error::HttpHeaderError;
pub use processor::HttpHeaderProcessor;
pub use types::{HttpHeaderParseState, HttpResponseHead};

/// Find the first byte of the `\r\n\r\n` sequence ending an HTTP header.
pub fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Parse one complete HTTP response header block, excluding any response body.
pub fn parse_response_head(bytes: &[u8]) -> Result<HttpResponseHead, HttpHeaderError> {
    let mut processor = HttpHeaderProcessor::new();
    match processor.feed(bytes) {
        HttpHeaderParseState::Complete => processor.get_result(),
        HttpHeaderParseState::Error(message) => Err(HttpHeaderError(message.clone())),
        HttpHeaderParseState::ParsingStatusLine | HttpHeaderParseState::ParsingHeaders => Err(
            HttpHeaderError("Incomplete HTTP response headers".to_string()),
        ),
    }
}

#[cfg(test)]
mod tests;
