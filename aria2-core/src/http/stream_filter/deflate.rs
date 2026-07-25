//! Deflate (RFC 1951) / Zlib (RFC 1950) decompressor with streaming support
//!
//! Implements deflate content-encoding decompression using the flate2 library.
//! Supports both raw deflate and zlib-wrapped deflate streams with true
//! incremental processing — data can be fed in arbitrary chunks.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;

/// Deflate/Zlib format decompressor with streaming support
///
/// Decompresses data encoded with Content-Encoding: deflate.
/// Per RFC 7230 Section 4.2, "deflate" means zlib format (RFC 1950)
/// which wraps deflate (RFC 1951) with a zlib header and checksum.
///
/// Some servers incorrectly send raw deflate without the zlib wrapper.
/// This decoder tries zlib first, and falls back to raw deflate on failure.
///
/// # Streaming Model
///
/// Uses `flate2::Decompress` for incremental processing:
/// 1. Each `filter()` call feeds input to the decompressor
/// 2. Output is produced immediately when decompressed data is available
/// 3. Unconsumed input is buffered for the next call
/// 4. The decompressor retains state between calls
#[derive(Debug)]
pub struct DeflateDecoder {
    /// Active zlib decompressor
    decompress: Option<flate2::Decompress>,
    /// Whether decompression is complete
    finished: bool,
    /// Whether we've tried zlib mode and need to fall back to raw deflate
    tried_zlib: bool,
    /// Whether we've confirmed raw deflate works
    using_raw: bool,
    /// Buffered input that hasn't been consumed yet
    input_buffer: Vec<u8>,
}

impl DeflateDecoder {
    /// Create a new Deflate decoder instance
    pub fn new() -> Self {
        DeflateDecoder {
            decompress: None,
            finished: false,
            tried_zlib: false,
            using_raw: false,
            input_buffer: Vec::new(),
        }
    }

    /// Try decompressing with the given format (zlib or raw deflate).
    ///
    /// Creates a new `Decompress` instance, feeds `data`, and stores the
    /// decompressor in `self.decompress` for subsequent incremental calls.
    /// Returns the output and whether the stream ended.
    fn try_new_decompress(&mut self, data: &[u8], zlib: bool) -> Result<(Vec<u8>, bool)> {
        let mut decompress = flate2::Decompress::new(zlib);
        let mut output = Vec::with_capacity(data.len().saturating_mul(3).max(256));

        let total_in_before = decompress.total_in() as usize;

        match decompress.decompress_vec(data, &mut output, flate2::FlushDecompress::None) {
            Ok(flate2::Status::Ok) => {
                // Partial decompression succeeded — keep the decompressor
                let consumed = decompress.total_in() as usize - total_in_before;
                if consumed < data.len() {
                    self.input_buffer = data[consumed..].to_vec();
                } else {
                    self.input_buffer.clear();
                }
                self.decompress = Some(decompress);
                Ok((output, false))
            }
            Ok(flate2::Status::StreamEnd) => {
                let consumed = decompress.total_in() as usize - total_in_before;
                if consumed < data.len() {
                    self.input_buffer = data[consumed..].to_vec();
                } else {
                    self.input_buffer.clear();
                }
                self.decompress = Some(decompress);
                Ok((output, true))
            }
            Ok(flate2::Status::BufError) => {
                // Need more data
                self.input_buffer.clear();
                self.decompress = Some(decompress);
                Ok((output, false))
            }
            Err(_) => Err(Aria2Error::Io("Decompression failed".to_string())),
        }
    }
}

impl Default for DeflateDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for DeflateDecoder {
    /// Process deflate/zlib compressed data incrementally.
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

        // Buffer incoming data
        self.input_buffer.extend_from_slice(input);
        let data = std::mem::take(&mut self.input_buffer);

        if let Some(ref mut decompress) = self.decompress {
            // We already have an active decompressor — continue feeding
            let total_in_before = decompress.total_in() as usize;
            let mut output = Vec::with_capacity(data.len().saturating_mul(3).max(256));

            match decompress.decompress_vec(&data, &mut output, flate2::FlushDecompress::None) {
                Ok(flate2::Status::Ok) => {
                    let consumed = decompress.total_in() as usize - total_in_before;
                    if consumed < data.len() {
                        self.input_buffer.extend_from_slice(&data[consumed..]);
                    }
                    Ok(output)
                }
                Ok(flate2::Status::StreamEnd) => {
                    self.finished = true;
                    let consumed = decompress.total_in() as usize - total_in_before;
                    if consumed < data.len() {
                        self.input_buffer.extend_from_slice(&data[consumed..]);
                    }
                    Ok(output)
                }
                Ok(flate2::Status::BufError) => {
                    self.input_buffer = data;
                    Ok(output)
                }
                Err(e) => {
                    self.input_buffer = data;
                    Err(Aria2Error::Io(format!("Deflate decompression failed: {}", e)))
                }
            }
        } else if !self.tried_zlib {
            // First attempt: try zlib format
            self.tried_zlib = true;
            match self.try_new_decompress(&data, true) {
                Ok((output, stream_end)) => {
                    if stream_end {
                        self.finished = true;
                    }
                    Ok(output)
                }
                Err(_) => {
                    // Zlib failed, try raw deflate
                    match self.try_new_decompress(&data, false) {
                        Ok((output, stream_end)) => {
                            self.using_raw = true;
                            if stream_end {
                                self.finished = true;
                            }
                            Ok(output)
                        }
                        Err(e) => {
                            self.input_buffer = data;
                            Err(Aria2Error::Io(format!(
                                "Deflate decompression failed (tried both zlib and raw): {}",
                                e
                            )))
                        }
                    }
                }
            }
        } else {
            // Already determined we need raw deflate
            match self.try_new_decompress(&data, false) {
                Ok((output, stream_end)) => {
                    self.using_raw = true;
                    if stream_end {
                        self.finished = true;
                    }
                    Ok(output)
                }
                Err(_) => {
                    self.input_buffer = data;
                    Ok(Vec::new())
                }
            }
        }
    }

    /// Flush the decoder buffer
    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished || self.input_buffer.is_empty() {
            return Ok(Vec::new());
        }

        let mut output = Vec::new();

        if let Some(ref mut decompress) = self.decompress {
            let _ = decompress.decompress_vec(
                &self.input_buffer,
                &mut output,
                flate2::FlushDecompress::Finish,
            );
            self.input_buffer.clear();
        } else {
            // No active decompressor — try zlib then raw deflate
            let data = std::mem::take(&mut self.input_buffer);
            if !self.tried_zlib {
                self.tried_zlib = true;
                if let Ok((out, _)) = self.try_new_decompress(&data, true) {
                    output = out;
                } else if let Ok((out, _)) = self.try_new_decompress(&data, false) {
                    self.using_raw = true;
                    output = out;
                }
            } else if let Ok((out, _)) = self.try_new_decompress(&data, false) {
                self.using_raw = true;
                output = out;
            }
        }

        self.finished = true;
        Ok(output)
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
        assert!(decoder.input_buffer.is_empty());
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
        let result = decoder.filter(&[0xFF, 0xFE, 0xFD, 0xFC]);
        assert!(result.is_err());
    }
}
