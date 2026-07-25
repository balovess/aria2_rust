//! BZip2 format decompressor with streaming support
//!
//! BZip2 data decompressor implemented using the bzip2-rs library.
//! Supports true streaming/incremental decompression where data can
//! be fed in arbitrary chunks.

use std::cell::Cell;
use std::io::Read;
use std::rc::Rc;

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
    /// Number of bytes consumed in the last filter() call
    bytes_processed: usize,
    /// Length of the input buffer before the last decompression attempt.
    /// Used to detect whether the decoder made any progress (avoid infinite loops).
    last_attempt_len: usize,
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
            bytes_processed: 0,
            last_attempt_len: 0,
        }
    }

    /// Number of input bytes consumed in the last `filter()` call.
    pub fn bytes_processed(&self) -> usize {
        self.bytes_processed
    }
}

impl Default for BZip2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// A `Read` wrapper that tracks how many bytes have been consumed
/// from the underlying slice via a shared `Rc<Cell<usize>>`.
struct TrackingReader<'a> {
    data: &'a [u8],
    pos: usize,
    consumed: Rc<Cell<usize>>,
}

impl<'a> TrackingReader<'a> {
    fn new(data: &'a [u8], consumed: Rc<Cell<usize>>) -> Self {
        Self {
            data,
            pos: 0,
            consumed,
        }
    }
}

impl<'a> Read for TrackingReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = &self.data[self.pos..];
        let to_read = buf.len().min(remaining.len());
        buf[..to_read].copy_from_slice(&remaining[..to_read]);
        self.pos += to_read;
        self.consumed.set(self.pos);
        if to_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "end of buffered input",
            ));
        }
        Ok(to_read)
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
        self.bytes_processed = 0;

        // Skip decompression if the buffer hasn't grown since last attempt
        // (prevents infinite loops on incomplete data)
        if self.input_buffer.len() == self.last_attempt_len {
            return Ok(Vec::new());
        }

        let buf_len_before = self.input_buffer.len();

        // Create a shared counter for tracking consumed bytes
        let consumed = Rc::new(Cell::new(0usize));
        let tracking = TrackingReader::new(&self.input_buffer, consumed.clone());
        let mut decoder = bzip2_rs::DecoderReader::new(tracking);

        let mut output = Vec::with_capacity(self.input_buffer.len().saturating_mul(3).max(256));

        match decoder.read_to_end(&mut output) {
            Ok(_) => {
                // Successfully decompressed all available data.
                let n = consumed.get();
                self.bytes_processed = n;
                self.input_buffer = self.input_buffer[n..].to_vec();
                self.last_attempt_len = self.input_buffer.len();

                if self.input_buffer.is_empty() {
                    self.finished = true;
                }
                Ok(output)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Incomplete stream — need more data.
                // Return any partial output we got.
                let n = consumed.get();
                self.bytes_processed = n;
                if n > 0 {
                    self.input_buffer = self.input_buffer[n..].to_vec();
                }
                self.last_attempt_len = self.input_buffer.len();
                Ok(output)
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::Other
                    && self.input_buffer.len() > buf_len_before =>
            {
                // bzip2-rs may return ErrorKind::Other for partial data.
                // Try to check if the decoder made progress.
                let n = consumed.get();
                self.bytes_processed = n;
                if n > 0 {
                    self.input_buffer = self.input_buffer[n..].to_vec();
                }
                self.last_attempt_len = self.input_buffer.len();
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
            let consumed = Rc::new(Cell::new(0usize));
            let tracking = TrackingReader::new(&self.input_buffer, consumed);
            let mut decoder = bzip2_rs::DecoderReader::new(tracking);
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
        use bzip2_rs::write::BzEncoder;
        let mut encoder = BzEncoder::new(Vec::new(), bzip2_rs::Compression::level(6));
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
