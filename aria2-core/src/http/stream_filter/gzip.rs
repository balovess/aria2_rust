//! Core compatibility adapter for the protocol GZip decoder.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;
use aria2_protocol::http::stream_filter::{
    GZipDecoder as ProtocolGZipDecoder, HttpStreamFilter, HttpStreamFilterError,
};

/// Incrementally decodes GZip, zlib, or raw Deflate response bodies.
#[derive(Debug)]
pub struct GZipDecoder {
    inner: ProtocolGZipDecoder,
}

impl GZipDecoder {
    pub fn new() -> Self {
        Self {
            inner: ProtocolGZipDecoder::new(),
        }
    }

    pub fn bytes_processed(&self) -> usize {
        self.inner.bytes_processed()
    }
}

impl Default for GZipDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for GZipDecoder {
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
