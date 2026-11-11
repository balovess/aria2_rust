//! Compatibility entrypoint for the protocol crate's streaming HTTP parser.

use crate::error::{Aria2Error, Result};
use aria2_protocol::http::header_processor::HttpHeaderProcessor as ProtocolHeaderProcessor;

pub use aria2_protocol::http::header_processor::{HttpHeaderParseState, HttpResponseHead};

#[cfg(test)]
mod tests;

/// Core-facing HTTP header parser retaining the historical `Aria2Error` result.
///
/// Parsing and response-head types live in `aria2-protocol`; this adapter keeps
/// the existing core API's error type for library callers.
pub struct HttpHeaderProcessor {
    inner: ProtocolHeaderProcessor,
    pub(crate) state: HttpHeaderParseState,
}

impl HttpHeaderProcessor {
    pub fn new() -> Self {
        Self {
            inner: ProtocolHeaderProcessor::new(),
            state: HttpHeaderParseState::ParsingStatusLine,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> &HttpHeaderParseState {
        self.state = self.inner.feed(data).clone();
        &self.state
    }

    pub fn last_bytes_processed(&self) -> usize {
        self.inner.last_bytes_processed()
    }

    pub fn get_result(&self) -> Result<HttpResponseHead> {
        self.inner
            .get_result()
            .map_err(|error| Aria2Error::HttpProtocol(error.to_string()))
    }

    pub fn get_header_string(&self) -> String {
        self.inner.get_header_string()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
        self.state = HttpHeaderParseState::ParsingStatusLine;
    }
}

impl Default for HttpHeaderProcessor {
    fn default() -> Self {
        Self::new()
    }
}
