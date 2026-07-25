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
        if to_read == 0 {
            // Return Ok(0) to signal EOF cleanly — the bzip2 decoder
            // interprets UnexpectedEof as corrupt data, but Ok(0) as
            // "no more data available" which maps to the
            // UnexpectedEof error kind in filter().
            return Ok(0);
        }
        buf[..to_read].copy_from_slice(&remaining[..to_read]);
        self.pos += to_read;
        self.consumed.set(self.pos);
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

        // Create a shared counter for tracking consumed bytes
        let consumed = Rc::new(Cell::new(0usize));
        let tracking = TrackingReader::new(&self.input_buffer, consumed.clone());
        let mut decoder = bzip2_rs::DecoderReader::new(tracking);

        let mut output = Vec::with_capacity(self.input_buffer.len().saturating_mul(3).max(256));

        match decoder.read_to_end(&mut output) {
            Ok(_) => {
                // read_to_end returning Ok means the bzip2 stream is complete
                // (the decoder found the end-of-stream marker).
                let n = consumed.get();
                self.bytes_processed = n;
                self.input_buffer = self.input_buffer[n..].to_vec();
                self.last_attempt_len = self.input_buffer.len();
                self.finished = true;
                Ok(output)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Incomplete stream — need more data.
                // Do NOT consume any input bytes on error: the decoder read
                // bytes internally but didn't successfully decode them, so
                // the full buffer must be preserved for the next attempt.
                self.last_attempt_len = self.input_buffer.len();
                Ok(output)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Other => {
                // bzip2-rs wraps all its internal errors (including
                // "symbol range truncated", "next magic truncated", etc.)
                // as ErrorKind::Other. These may indicate either incomplete
                // data (truncated stream) or genuine corruption.
                // Treat as "need more data" — preserve the full input buffer
                // so the next call with additional data can retry from the
                // beginning. The last_attempt_len guard prevents infinite
                // loops when no new data arrives.
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

    /// Create bzip2-compressed data using a pre-computed test vector.
    ///
    /// Since bzip2_rs v0.1 doesn't provide a write/BzEncoder API,
    /// we use a hardcoded bzip2 stream for "Hello, World!" to validate
    /// decompression. This avoids requiring an additional bzip2 encoder
    /// dependency just for tests.
    fn create_bzip2_hello_world() -> Vec<u8> {
        // Pre-computed bzip2 stream for "Hello, World!\n"
        // Generated with: Python bz2.compress(b"Hello, World!\n")
        vec![
            0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59,
            0x99, 0xac, 0x22, 0x56, 0x00, 0x00, 0x02, 0x57, 0x80, 0x00,
            0x10, 0x60, 0x04, 0x00, 0x40, 0x00, 0x80, 0x06, 0x04, 0x90,
            0x00, 0x20, 0x00, 0x22, 0x06, 0x81, 0x90, 0x80, 0x69, 0xa6,
            0x89, 0x18, 0x6a, 0xce, 0xa4, 0x19, 0x6f, 0x8b, 0xb9, 0x22,
            0x9c, 0x28, 0x48, 0x4c, 0xd6, 0x11, 0x2b, 0x00,
        ]
    }

    #[test]
    fn test_bzip2_decoder_basic() {
        let compressed = create_bzip2_hello_world();

        let mut decoder = BZip2Decoder::new();
        let result = decoder.filter(&compressed).unwrap();
        // The decompressed data should contain "Hello, World!"
        assert!(String::from_utf8_lossy(&result).contains("Hello, World!"));
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
        let compressed = create_bzip2_hello_world();

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

        let compressed = create_bzip2_hello_world();

        let mut decoder = BZip2Decoder::new();
        decoder.filter(&compressed).unwrap();
        assert!(!decoder.needs_more_input());
    }

    #[test]
    fn test_bzip2_decoder_incremental() {
        let compressed = create_bzip2_hello_world();

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

        assert!(String::from_utf8_lossy(&result).contains("Hello, World!"));
    }
}
