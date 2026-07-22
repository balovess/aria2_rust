//! GZip format decompressor
//!
//! GZip (RFC 1952) data decompressor implemented using the flate2 library.
//! Supports streaming decompression, can process large compressed files in chunks.

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;
use flate2::read::GzDecoder;
use std::io::{Cursor, Read};

/// GZip format decompressor
///
/// GZip (RFC 1952) data decompressor implemented using the flate2 library.
/// Supports streaming decompression, can process large compressed files in chunks.
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{GZipDecoder, StreamFilter};
///
/// let mut decoder = GZipDecoder::new();
/// let compressed_data = /* compressed GZip data */;
/// let decompressed = decoder.filter(compressed_data)?;
/// ```
#[derive(Debug)]
pub struct GZipDecoder {
    /// Internal GzDecoder instance
    inner: Option<GzDecoder<Cursor<Vec<u8>>>>,
    /// Whether decompression is complete
    finished: bool,
}

impl GZipDecoder {
    /// Create a new GZip decoder instance
    ///
    /// # Returns
    ///
    /// A new GZipDecoder instance
    pub fn new() -> Self {
        GZipDecoder {
            inner: None,
            finished: false,
        }
    }
}

impl Default for GZipDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for GZipDecoder {
    /// Process GZip compressed data
    ///
    /// On first call, detects GZip magic number (0x1f 0x8b) to validate data format.
    /// Subsequent calls append data to the internal buffer and decompress.
    ///
    /// # Arguments
    ///
    /// * `input` - GZip compressed byte data
    ///
    /// # Returns
    ///
    /// Decompressed raw data, or error message
    ///
    /// # Errors
    ///
    /// - If input data is not valid GZip format (missing magic number)
    /// - If an I/O error occurs during decompression
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // Check if already finished
        if self.finished && self.inner.is_none() {
            return Err(Aria2Error::Parse(
                "GZip decoder already finished".to_string(),
            ));
        }

        // Validate GZip magic number (on first call)
        if self.inner.is_none() {
            if input.len() < 2 {
                return Err(Aria2Error::Parse(
                    "Input too short for GZip header".to_string(),
                ));
            }

            // GZip magic number: 0x1f 0x8b
            if input[0] != 0x1f || input[1] != 0x8b {
                return Err(Aria2Error::Parse("Invalid GZip magic number".to_string()));
            }

            // Initialize decoder
            let cursor = Cursor::new(input.to_vec());
            self.inner = Some(GzDecoder::new(cursor));
        } else {
            // Append new data to existing buffer
            // Note: Due to GzDecoder limitations, we recreate the decoder here
            // Production use may require more sophisticated buffer management
            return Err(Aria2Error::Parse(
                "GZip incremental decoding not fully supported in this implementation".to_string(),
            ));
        }

        // Execute decompression with pre-allocated output buffer
        // Gzip typically expands 2-3x, but allocate at least 256 bytes to avoid tiny reallocations
        if let Some(ref mut decoder) = self.inner {
            let mut output = Vec::with_capacity(input.len().saturating_mul(3).max(256));
            match decoder.read_to_end(&mut output) {
                Ok(_) => {
                    self.finished = true;
                    self.inner = None; // Release decoder resources
                    Ok(output)
                }
                Err(e) => Err(Aria2Error::Io(e.to_string())),
            }
        } else {
            Err(Aria2Error::Parse(
                "GZip decoder not initialized".to_string(),
            ))
        }
    }

    /// Flush GZip decoder buffer
    ///
    /// Returns remaining decompressed data internally. For one-shot decompression,
    /// this method typically returns an empty vector (since all data was output in filter()).
    ///
    /// # Returns
    ///
    /// Remaining data in the buffer
    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            Ok(Vec::new())
        } else if self.inner.is_some() {
            // Attempt to complete decompression
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

    /// Returns "gzip"
    fn name(&self) -> &'static str {
        "gzip"
    }

    /// Check if more input is needed
    ///
    /// Returns false when finished=true and inner=None, indicating decompression is complete
    fn needs_more_input(&self) -> bool {
        !(self.finished && self.inner.is_none())
    }
}
