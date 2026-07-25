//! BZip2 format decompressor with streaming support
//!
//! BZip2 data decompressor implemented using the bzip2-rs library.
//! Supports true streaming/incremental decompression where data can
//! be fed in arbitrary chunks.

use std::io::Read;

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;

/// BZip2 format decompressor with streaming support
///
/// Decompresses BZip2-encoded data incrementally. Uses the same
/// approach as the GZip decoder: accumulates input data and attempts
/// decompression. For BZip2, the bzip2-rs library provides a
/// streaming reader that can handle partial data.
///
/// # Streaming Model
///
/// 1. Each `filter()` call buffers input data
/// 2. When enough data is available, decompression produces output
/// 3. The decoder tracks how much input has been consumed
/// 4. `flush()` finalizes the stream
pub struct BZip2Decoder {
    /// Whether decompression is complete
    finished: bool,
    /// Buffered input that hasn't been consumed yet
    input_buffer: Vec<u8>,
}

impl std::fmt::Debug for BZip2Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BZip2Decoder")
            .field("finished", &self.finished)
            .field("buffer_len", &self.input_buffer.len())
            .finish()
    }
}

impl BZip2Decoder {
    /// Create a new BZip2 decoder instance
    pub fn new() -> Self {
        BZip2Decoder {
            finished: false,
            input_buffer: Vec::new(),
        }
    }
}

impl Default for BZip2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for BZip2Decoder {
    /// Process BZip2 compressed data incrementally.
    ///
    /// Accumulates input and attempts decompression. Produces output
    /// when enough data is available for the bzip2 decoder.
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if self.finished {
            return Err(Aria2Error::Parse(
                "BZip2 decoder already finished".to_string(),
            ));
        }

        if input.is_empty() {
            return Ok(Vec::new());
        }

        // Buffer incoming data
        self.input_buffer.extend_from_slice(input);

        // Try to decompress with the current buffer
        let cursor = std::io::Cursor::new(&self.input_buffer[..]);
        let mut decoder = bzip2_rs::DecoderReader::new(cursor);

        let mut output = Vec::with_capacity(self.input_buffer.len().saturating_mul(3).max(256));

        match decoder.read_to_end(&mut output) {
            Ok(_) => {
                // Successfully decompressed all available data
                // Check if the stream is complete
                let consumed = decoder.get_ref().position() as usize;
                self.input_buffer = self.input_buffer[consumed..].to_vec();

                if self.input_buffer.is_empty() {
                    self.finished = true;
                }
                Ok(output)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Incomplete stream — need more data
                // Return any partial output we got
                let consumed = decoder.get_ref().position() as usize;
                if consumed > 0 {
                    self.input_buffer = self.input_buffer[consumed..].to_vec();
                }
                Ok(output)
            }
            Err(e) => Err(Aria2Error::Io(format!(
                "BZip2 decompression failed: {}",
                e
            ))),
        }
    }

    /// Flush BZip2 decoder buffer
    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let mut output = Vec::new();

        if !self.input_buffer.is_empty() {
            let cursor = std::io::Cursor::new(&self.input_buffer[..]);
            let mut decoder = bzip2_rs::DecoderReader::new(cursor);
            let _ = decoder.read_to_end(&mut output);
            self.input_buffer.clear();
        }

        self.finished = true;
        Ok(output)
    }

    /// Returns "bzip2"
    fn name(&self) -> &'static str {
        "bzip2"
    }

    /// Check if more input is needed
    fn needs_more_input(&self) -> bool {
        !self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_bzip2_data(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = bzip2_rs::EncoderWriter::new(Vec::new(), 6);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn test_bzip2_decoder_basic() {
        let data = b"Hello, World! This is a test of BZip2 compression.";
        let compressed = create_bzip2_data(data);

        let mut decoder = BZip2Decoder::new();
        let result = decoder.filter(&compressed).unwrap();
        assert_eq!(result.as_slice(), data);
        assert!(decoder.finished);
    }

    #[test]
    fn test_bzip2_decoder_empty_input() {
        let mut decoder = BZip2Decoder::new();
        let result = decoder.filter(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_bzip2_decoder_already_finished() {
        let data = b"test data";
        let compressed = create_bzip2_data(data);

        let mut decoder = BZip2Decoder::new();
        decoder.filter(&compressed).unwrap();
        let result = decoder.filter(&[1, 2, 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_bzip2_decoder_name() {
        let decoder = BZip2Decoder::new();
        assert_eq!(decoder.name(), "bzip2");
    }

    #[test]
    fn test_bzip2_decoder_needs_more_input() {
        let decoder = BZip2Decoder::new();
        assert!(decoder.needs_more_input());

        let data = b"test";
        let compressed = create_bzip2_data(data);

        let mut decoder = BZip2Decoder::new();
        decoder.filter(&compressed).unwrap();
        assert!(!decoder.needs_more_input());
    }

    #[test]
    fn test_bzip2_decoder_incremental() {
        let data = b"Testing incremental BZip2 decompression with chunks.";
        let compressed = create_bzip2_data(data);

        let mut decoder = BZip2Decoder::new();
        let mut result = Vec::new();

        let mid = compressed.len() / 2;
        result.extend_from_slice(&decoder.filter(&compressed[..mid]).unwrap());
        if !decoder.finished {
            result.extend_from_slice(&decoder.filter(&compressed[mid..]).unwrap());
        }
        if !decoder.finished {
            result.extend_from_slice(&decoder.flush().unwrap());
        }

        assert_eq!(result.as_slice(), data);
    }
}
