//! Core compatibility adapter for the protocol BZip2 decoder.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;
use aria2_protocol::http::stream_filter::{
    BZip2Decoder as ProtocolBZip2Decoder, HttpStreamFilter, HttpStreamFilterError,
};

/// Incrementally decodes BZip2 response bodies.
pub struct BZip2Decoder {
    inner: ProtocolBZip2Decoder,
}

impl std::fmt::Debug for BZip2Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BZip2Decoder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl BZip2Decoder {
    pub fn new() -> Self {
        Self {
            inner: ProtocolBZip2Decoder::new(),
        }
    }

    pub fn bytes_processed(&self) -> usize {
        self.inner.bytes_processed()
    }
}

impl Default for BZip2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for BZip2Decoder {
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        self.inner.filter(input).map_err(map_error)
    }

    fn flush(&mut self) -> Result<Vec<u8>> {
        self.inner.flush().map_err(map_error)
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn needs_more_input(&self) -> bool {
        self.inner.needs_more_input()
    }
}

fn map_error(error: HttpStreamFilterError) -> Aria2Error {
    match error {
        HttpStreamFilterError::Parse(message) => Aria2Error::Parse(message),
        HttpStreamFilterError::Io(message) => Aria2Error::Io(message),
        HttpStreamFilterError::Protocol(message) => Aria2Error::HttpProtocol(message),
    }
}
