//! Core compatibility adapter for the protocol Deflate decoder.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;
use aria2_protocol::http::stream_filter::{
    DeflateDecoder as ProtocolDeflateDecoder, HttpStreamFilter, HttpStreamFilterError,
};

/// Incrementally decodes zlib-wrapped or raw Deflate response bodies.
#[derive(Debug)]
pub struct DeflateDecoder {
    inner: ProtocolDeflateDecoder,
}

impl DeflateDecoder {
    pub fn new() -> Self {
        Self {
            inner: ProtocolDeflateDecoder::new(),
        }
    }
}

impl Default for DeflateDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for DeflateDecoder {
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
