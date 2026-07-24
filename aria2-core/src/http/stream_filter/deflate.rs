//! Deflate (RFC 1951) / Zlib (RFC 1950) decompressor
//!
//! Implements deflate content-encoding decompression using the flate2 library.
//! Supports both raw deflate and zlib-wrapped deflate streams.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;
use flate2::read::ZlibDecoder;
use std::io::{Cursor, Read};

/// Deflate/Zlib format decompressor
///
/// Decompresses data encoded with Content-Encoding: deflate.
/// Per RFC 7230 Section 4.2, "deflate" means zlib format (RFC 1950)
/// which wraps deflate (RFC 1951) with a zlib header and checksum.
///
/// Some servers incorrectly send raw deflate without the zlib wrapper.
/// This decoder tries zlib first, and falls back to raw deflate on failure.
#[derive(Debug)]
pub struct DeflateDecoder {
    /// Internal ZlibDecoder instance
    inner: Option<ZlibDecoder<Cursor<Vec<u8>>>>,
    /// Whether decompression is complete
    finished: bool,
    /// Whether we've tried zlib mode and need to fall back to raw deflate
    tried_zlib: bool,
    /// Buffered raw data for retry with raw deflate
    raw_buffer: Vec<u8>,
}

impl DeflateDecoder {
    /// Create a new Deflate decoder instance
    pub fn new() -> Self {
        DeflateDecoder {
            inner: None,
            finished: false,
            tried_zlib: false,
            raw_buffer: Vec::new(),
        }
    }

    /// Try to decompress data using zlib format
    fn try_zlib(data: &[u8]) -> Result<Vec<u8>> {
        let cursor = Cursor::new(data.to_vec());
        let mut decoder = ZlibDecoder::new(cursor);
        let mut output = Vec::with_capacity(data.len().saturating_mul(3).max(256));
        decoder
            .read_to_end(&mut output)
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        Ok(output)
    }

    /// Try to decompress data using raw deflate format (no zlib header)
    fn try_raw_deflate(data: &[u8]) -> Result<Vec<u8>> {
        use flate2::read::DeflateDecoder as RawDeflateDecoder;
        let cursor = Cursor::new(data.to_vec());
        let mut decoder = RawDeflateDecoder::new(cursor);
        let mut output = Vec::with_capacity(data.len().saturating_mul(3).max(256));
        decoder
            .read_to_end(&mut output)
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        Ok(output)
    }
}

impl Default for DeflateDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for DeflateDecoder {
    /// Process deflate/zlib compressed data
    ///
    /// Tries zlib format first (standard per RFC 7230), then falls back
    /// to raw deflate if zlib decoding fails. This handles servers that
    /// incorrectly send raw deflate without the zlib wrapper.
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if self.finished {
            return Err(Aria2Error::Parse(
                "Deflate decoder already finished".to_string(),
            ));
        }

        if input.is_empty() {
            return Ok(Vec::new());
        }

        // Buffer all incoming data for potential retry
        self.raw_buffer.extend_from_slice(input);

        if self.inner.is_none() && !self.tried_zlib {
            // First attempt: try zlib format (RFC 1950 - the correct one per HTTP spec)
            match Self::try_zlib(&self.raw_buffer) {
                Ok(output) => {
                    self.finished = true;
                    return Ok(output);
                }
                Err(_) => {
                    // Zlib failed, try raw deflate (some servers send this incorrectly)
                    tracing::debug!("Zlib decode failed, trying raw deflate fallback");
                    self.tried_zlib = true;
                    match Self::try_raw_deflate(&self.raw_buffer) {
                        Ok(output) => {
                            self.finished = true;
                            return Ok(output);
                        }
                        Err(e) => {
                            // Both failed - return error
                            return Err(Aria2Error::Io(format!(
                                "Deflate decompression failed (tried both zlib and raw): {}",
                                e
                            )));
                        }
                    }
                }
            }
        } else if self.tried_zlib && !self.finished {
            // Already determined we need raw deflate, try again with accumulated data
            match Self::try_raw_deflate(&self.raw_buffer) {
                Ok(output) => {
                    self.finished = true;
                    Ok(output)
                }
                Err(_) => {
                    // Not enough data yet, need more input
                    Ok(Vec::new())
                }
            }
        } else {
            Ok(Vec::new())
        }
    }

    /// Flush the decoder buffer
    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished || self.raw_buffer.is_empty() {
            return Ok(Vec::new());
        }

        // Try one last time to decompress whatever we have
        if !self.tried_zlib {
            if let Ok(output) = Self::try_zlib(&self.raw_buffer) {
                self.finished = true;
                return Ok(output);
            }
            self.tried_zlib = true;
        }

        match Self::try_raw_deflate(&self.raw_buffer) {
            Ok(output) => {
                self.finished = true;
                Ok(output)
            }
            Err(e) => {
                tracing::warn!("Deflate flush failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    /// Returns "deflate"
    fn name(&self) -> &'static str {
        "deflate"
    }

    /// Check if more input is needed
    fn needs_more_input(&self) -> bool {
        !self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::{DeflateEncoder, ZlibEncoder};
    use flate2::Compression;
    use std::io::Write;

    #[test]
    fn test_deflate_decoder_new() {
        let decoder = DeflateDecoder::new();
        assert!(!decoder.finished);
        assert!(!decoder.tried_zlib);
        assert!(decoder.raw_buffer.is_empty());
    }

    #[test]
    fn test_deflate_decoder_default() {
        let decoder = DeflateDecoder::default();
        assert!(!decoder.finished);
    }

    #[test]
    fn test_deflate_zlib_encoded() {
        let data = b"Hello, World! This is a test of deflate compression.";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut decoder = DeflateDecoder::new();
        let result = decoder.filter(&compressed).unwrap();
        assert_eq!(result.as_slice(), data);
        assert!(decoder.finished);
    }

    #[test]
    fn test_deflate_raw_encoded() {
        let data = b"Hello, World! Raw deflate test.";
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut decoder = DeflateDecoder::new();
        let result = decoder.filter(&compressed).unwrap();
        assert_eq!(result.as_slice(), data);
        assert!(decoder.finished);
    }

    #[test]
    fn test_deflate_empty_input() {
        let mut decoder = DeflateDecoder::new();
        let result = decoder.filter(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_deflate_already_finished() {
        let data = b"test data";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut decoder = DeflateDecoder::new();
        decoder.filter(&compressed).unwrap();

        let result = decoder.filter(&[1, 2, 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_deflate_name() {
        let decoder = DeflateDecoder::new();
        assert_eq!(decoder.name(), "deflate");
    }

    #[test]
    fn test_deflate_needs_more_input() {
        let decoder = DeflateDecoder::new();
        assert!(decoder.needs_more_input());

        let data = b"test";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut decoder = DeflateDecoder::new();
        decoder.filter(&compressed).unwrap();
        assert!(!decoder.needs_more_input());
    }

    #[test]
    fn test_deflate_flush_empty() {
        let mut decoder = DeflateDecoder::new();
        let result = decoder.flush().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_deflate_invalid_data() {
        let mut decoder = DeflateDecoder::new();
        // Random bytes are very unlikely to be valid deflate
        let result = decoder.filter(&[0xFF, 0xFE, 0xFD, 0xFC]);
        assert!(result.is_err());
    }
}
