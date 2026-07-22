//! BZip2 format decompressor
//!
//! BZip2 data decompressor implemented using the bzip2 library.
//! Similar to GZipDecoder, but uses the bzip2 compression algorithm.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;
use bzip2_rs::DecoderReader as BzDecoder;
use std::io::{Cursor, Read};

/// BZip2 format decompressor
///
/// BZip2 data decompressor implemented using the bzip2 library.
/// Similar to GZipDecoder, but uses the bzip2 compression algorithm.
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{BZip2Decoder, StreamFilter};
///
/// let mut decoder = BZip2Decoder::new();
/// let compressed_data = /* BZip2 compressed data */;
/// let decompressed = decoder.filter(compressed_data)?;
/// ```
pub struct BZip2Decoder {
    inner: Option<BzDecoder<Cursor<Vec<u8>>>>,
    finished: bool,
}

impl std::fmt::Debug for BZip2Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BZip2Decoder")
            .field("finished", &self.finished)
            .finish()
    }
}

impl BZip2Decoder {
    /// Create a new BZip2 decoder instance
    ///
    /// # Returns
    ///
    /// A new BZip2Decoder instance
    pub fn new() -> Self {
        BZip2Decoder {
            inner: None,
            finished: false,
        }
    }
}

impl Default for BZip2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for BZip2Decoder {
    /// Process BZip2 compressed data
    ///
    /// On first call, initializes the decoder and performs decompression.
    ///
    /// # Arguments
    ///
    /// * `input` - BZip2 compressed byte data
    ///
    /// # Returns
    ///
    /// Decompressed raw data, or error message
    ///
    /// # Errors
    ///
    /// - If input data is not valid BZip2 format
    /// - If an I/O error occurs during decompression
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // Check if already finished
        if self.finished && self.inner.is_none() {
            return Err(Aria2Error::Parse(
                "BZip2 decoder already finished".to_string(),
            ));
        }

        // Validate minimum length
        if input.len() < 10 {
            return Err(Aria2Error::Parse(
                "Input too short for BZip2 header".to_string(),
            ));
        }

        // Initialize decoder
        if self.inner.is_none() {
            let cursor = Cursor::new(input.to_vec());
            self.inner = Some(BzDecoder::new(cursor));
        } else {
            return Err(Aria2Error::Parse(
                "BZip2 incremental decoding not supported in this implementation".to_string(),
            ));
        }

        // Execute decompression
        if let Some(ref mut decoder) = self.inner {
            let mut output = Vec::new();
            match decoder.read_to_end(&mut output) {
                Ok(_) => {
                    self.finished = true;
                    Ok(output)
                }
                Err(e) => Err(Aria2Error::Io(e.to_string())),
            }
        } else {
            Err(Aria2Error::Parse(
                "BZip2 decoder not initialized".to_string(),
            ))
        }
    }

    /// Flush BZip2 decoder buffer
    ///
    /// # Returns
    ///
    /// Remaining data in the buffer
    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            Ok(Vec::new())
        } else if self.inner.is_some() {
            let mut output = Vec::new();
            if let Some(ref mut decoder) = self.inner {
                let _ = decoder.read_to_end(&mut output);
            }
            self.finished = true;
            Ok(output)
        } else {
            Ok(Vec::new())
        }
    }

    /// Returns "bzip2"
    fn name(&self) -> &'static str {
        "bzip2"
    }

    /// Check if more input is needed
    fn needs_more_input(&self) -> bool {
        !(self.finished && self.inner.is_none())
    }
}
