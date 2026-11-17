//! Core compatibility adapter for the protocol chunked transfer decoder.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;
use aria2_protocol::http::stream_filter::{
    ChunkedDecoder as ProtocolChunkedDecoder, HttpStreamFilter, HttpStreamFilterError,
};

/// Incrementally decodes HTTP chunked transfer framing.
#[derive(Debug)]
pub struct ChunkedDecoder {
    inner: ProtocolChunkedDecoder,
}

impl ChunkedDecoder {
    pub fn new() -> Self {
        Self {
            inner: ProtocolChunkedDecoder::new(),
        }
    }
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for ChunkedDecoder {
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
