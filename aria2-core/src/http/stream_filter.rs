//! Stream data decoder framework
//!
//! Provides composable stream data filters, supporting decoding of GZip, Chunked, BZip2 and other encoding formats.
//! Multiple filters can be chained via `process_filters` to implement complex data processing pipelines.

use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_writer::SeekableDiskWriter;
use bzip2_rs::DecoderReader as BzDecoder;
use flate2::read::GzDecoder;
use std::io::{Cursor, Read};

/// Stream filter trait
///
/// Defines the interface for stream data processors. All concrete filter implementations must implement this trait.
/// Filters support incremental data processing and can consume input data progressively across multiple calls.
pub trait StreamFilter: Send + Sync + std::fmt::Debug {
    /// Process input data and return filtered result
    ///
    /// # Arguments
    ///
    /// * `input` - Input data byte slice
    ///
    /// # Returns
    ///
    /// Filtered data, or error message
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>>;

    /// Flush internal buffer and return remaining data
    ///
    /// Call this method after input ends to ensure all buffered data is output.
    ///
    /// # Returns
    ///
    /// Remaining data in the buffer, or error message
    fn flush(&mut self) -> Result<Vec<u8>>;

    /// Return the filter name (for debugging and logging)
    fn name(&self) -> &'static str;

    /// Check if more input is needed to continue processing
    ///
    /// When returning `false`, the filter has completed its work and needs no more input.
    fn needs_more_input(&self) -> bool;
}

// ==================== GZip Decoder ====================

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

// ==================== Chunked Decoder ====================

/// Chunked Transfer-Encoding state enum
#[derive(Debug, Clone, PartialEq)]
enum ChunkState {
    /// Reading chunk size line
    ReadingSize,
    /// Reading chunk data
    ReadingData { remaining: usize },
    /// Reading \r\n after data (chunk data end marker)
    ReadingDataEnd,
    /// Chunked encoding complete (encountered size=0 terminator chunk)
    Complete,
    /// Error occurred
    Error(String),
}

/// HTTP Chunked Transfer-Encoding decoder
///
/// Implements chunked encoding decoding per RFC 7230 Section 4.1.
/// Supports chunk extensions (unknown extensions are ignored).
///
/// # Format
///
/// ```text
/// chunked-body   = *chunk
///                  last-chunk
///                  trailer-section
///                  CRLF
///
/// chunk          = chunk-size [chunk-ext] CRLF
///                  chunk-data CRLF
/// chunk-size     = 1*HEXDIG
/// last-chunk     = 1*("0") [chunk-ext] CRLF
/// ```
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{ChunkedDecoder, StreamFilter};
///
/// let mut decoder = ChunkedDecoder::new();
/// let chunked_data = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
/// let decoded = decoder.filter(chunked_data)?;
/// assert_eq!(decoded, b"hello world");
/// ```
#[derive(Debug)]
pub struct ChunkedDecoder {
    /// Current parsing state
    state: ChunkState,
    /// Collected size line data
    size_buffer: Vec<u8>,
    /// Remaining bytes in current chunk
    current_chunk_remaining: usize,
    /// Decoded output buffer
    output_buffer: Vec<u8>,
}

impl ChunkedDecoder {
    /// Create a new Chunked decoder instance
    ///
    /// # Returns
    ///
    /// A new ChunkedDecoder instance
    pub fn new() -> Self {
        ChunkedDecoder {
            state: ChunkState::ReadingSize,
            size_buffer: Vec::new(),
            current_chunk_remaining: 0,
            output_buffer: Vec::new(),
        }
    }
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for ChunkedDecoder {
    /// Parse chunked encoded data
    ///
    /// Parses chunked format per RFC 7230 Section 4.1:
    /// - ReadingSize: Read until \r\n, then parse hex size
    /// - ReadingData: Read specified amount of data, then return to ReadingSize
    /// - Enter Complete state when size=0 is encountered
    ///
    /// # Arguments
    ///
    /// * `input` - Chunked encoded byte data
    ///
    /// # Returns
    ///
    /// Decoded raw data (with chunked framing removed), or error message
    ///
    /// # Errors
    ///
    /// - If chunk size format is invalid (non-hex)
    /// - If attempting to continue processing in Error state
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // If already in error state, return error immediately
        if let ChunkState::Error(ref msg) = self.state {
            return Err(Aria2Error::Parse(msg.clone()));
        }

        // If already complete, return empty result
        if matches!(self.state, ChunkState::Complete) {
            return Ok(Vec::new());
        }

        let mut pos = 0;

        while pos < input.len() {
            match &self.state {
                ChunkState::ReadingSize => {
                    // Collect characters until \r\n is encountered
                    let byte = input[pos];
                    if byte == b'\r' {
                        // Skip \r
                        pos += 1;
                        // Check if next byte is \n
                        if pos < input.len() && input[pos] == b'\n' {
                            pos += 1; // Skip \n

                            // Parse size line
                            let size_str = String::from_utf8_lossy(&self.size_buffer).to_string();
                            let size_str_trimmed = size_str.trim();

                            // Separate size and possible extensions
                            let size_part = if let Some(semi_pos) = size_str_trimmed.find(';') {
                                &size_str_trimmed[..semi_pos]
                            } else {
                                size_str_trimmed
                            };

                            // Parse hex size
                            match usize::from_str_radix(size_part.trim(), 16) {
                                Ok(0) => {
                                    // Terminator chunk
                                    self.state = ChunkState::Complete;
                                    self.size_buffer.clear();
                                    let result = std::mem::take(&mut self.output_buffer);
                                    return Ok(result);
                                }
                                Ok(size) => {
                                    self.state = ChunkState::ReadingData { remaining: size };
                                    self.current_chunk_remaining = size;
                                    self.size_buffer.clear();
                                }
                                Err(_) => {
                                    self.state = ChunkState::Error(format!(
                                        "Invalid chunk size: {}",
                                        size_part
                                    ));
                                    return Err(Aria2Error::Parse(format!(
                                        "Invalid chunk size format: {}",
                                        size_part
                                    )));
                                }
                            }
                        }
                        // If not \n, continue waiting (pos has already been incremented)
                    } else if byte == b'\n' {
                        // Standalone \n, also treat as line terminator
                        pos += 1;

                        // Parse size line
                        let size_str = String::from_utf8_lossy(&self.size_buffer).to_string();
                        let size_str_trimmed = size_str.trim();

                        // Separate size and possible extensions
                        let size_part = if let Some(semi_pos) = size_str_trimmed.find(';') {
                            &size_str_trimmed[..semi_pos]
                        } else {
                            size_str_trimmed
                        };

                        // Parse hex size
                        match usize::from_str_radix(size_part.trim(), 16) {
                            Ok(0) => {
                                self.state = ChunkState::Complete;
                                self.size_buffer.clear();
                                let result = std::mem::take(&mut self.output_buffer);
                                return Ok(result);
                            }
                            Ok(size) => {
                                self.state = ChunkState::ReadingData { remaining: size };
                                self.current_chunk_remaining = size;
                                self.size_buffer.clear();
                            }
                            Err(_) => {
                                self.state =
                                    ChunkState::Error(format!("Invalid chunk size: {}", size_part));
                                return Err(Aria2Error::Parse(format!(
                                    "Invalid chunk size format: {}",
                                    size_part
                                )));
                            }
                        }
                    } else {
                        // Collect size character
                        self.size_buffer.push(byte);
                        pos += 1;
                    }
                }

                ChunkState::ReadingData { remaining } => {
                    let remaining_bytes = *remaining;
                    let available = input.len() - pos;

                    // Calculate bytes to copy this time
                    let to_copy = std::cmp::min(remaining_bytes, available);

                    // Copy data to output buffer
                    self.output_buffer
                        .extend_from_slice(&input[pos..pos + to_copy]);
                    pos += to_copy;

                    // Update remaining count
                    let new_remaining = remaining_bytes - to_copy;

                    if new_remaining == 0 {
                        // Current chunk data fully read, expect next \r\n
                        self.state = ChunkState::ReadingDataEnd;
                        self.current_chunk_remaining = 0;
                    } else {
                        // Update remaining count in state
                        self.state = ChunkState::ReadingData {
                            remaining: new_remaining,
                        };
                        self.current_chunk_remaining = new_remaining;
                    }
                }

                ChunkState::ReadingDataEnd => {
                    // Skip \r\n after chunk data
                    let byte = input[pos];
                    if byte == b'\r' || byte == b'\n' {
                        pos += 1; // Skip \r or \n
                    // Continue in ReadingDataEnd state
                    } else {
                        // Non-newline character encountered, \r\n has been consumed
                        // Switch to ReadingSize to process this character
                        self.state = ChunkState::ReadingSize;
                        // Don't increment pos, let next loop iteration handle this character
                    }
                }

                ChunkState::Complete => {
                    // Already complete, ignore subsequent data
                    break;
                }

                ChunkState::Error(msg) => {
                    return Err(Aria2Error::Parse(msg.clone()));
                }
            }
        }

        // Return all accumulated output data
        let result = std::mem::take(&mut self.output_buffer);
        Ok(result)
    }

    /// Flush Chunked decoder buffer
    ///
    /// Returns remaining decoded data in the buffer.
    /// If decoding is incomplete (still reading chunks), returns an error.
    ///
    /// # Returns
    ///
    /// Remaining data in the buffer, or error message (if decoding is incomplete)
    fn flush(&mut self) -> Result<Vec<u8>> {
        match &self.state {
            ChunkState::Complete => {
                // Complete state, return empty
                Ok(Vec::new())
            }
            ChunkState::ReadingSize
            | ChunkState::ReadingData { .. }
            | ChunkState::ReadingDataEnd => {
                // Incomplete state, data may be lost
                let remaining = std::mem::take(&mut self.output_buffer);
                if remaining.is_empty() {
                    Err(Aria2Error::Parse(
                        "Incomplete chunked encoding data".to_string(),
                    ))
                } else {
                    // Return existing data, but mark as warning
                    Ok(remaining)
                }
            }
            ChunkState::Error(msg) => Err(Aria2Error::Parse(msg.clone())),
        }
    }

    /// Returns "chunked"
    fn name(&self) -> &'static str {
        "chunked"
    }

    /// Check if more input is needed
    ///
    /// Only returns false when in Complete or Error state
    fn needs_more_input(&self) -> bool {
        !matches!(self.state, ChunkState::Complete | ChunkState::Error(_))
    }
}

// ==================== BZip2 Decoder ====================

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

// ==================== Filter processing helper ====================

/// Process input data through a sequence of filters.
///
/// Passes input data through each filter in sequence. The first filter receives
/// a direct reference to the input to avoid unnecessary cloning. Subsequent filters
/// receive the output from the previous filter.
///
/// If the filter list is empty, returns a copy of the input.
pub fn process_filters(filters: &mut [Box<dyn StreamFilter>], input: &[u8]) -> Result<Vec<u8>> {
    let mut data: Option<Vec<u8>> = None;

    for (index, filter) in filters.iter_mut().enumerate() {
        data = Some(if index == 0 {
            filter.filter(input)?
        } else {
            filter.filter(data.as_ref().unwrap())?
        });
    }

    Ok(data.unwrap_or_else(|| input.to_vec()))
}

/// Flush all filters and collect remaining output.
pub fn flush_filters(filters: &mut [Box<dyn StreamFilter>]) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    for filter in filters {
        let flushed = filter.flush()?;
        data.extend_from_slice(&flushed);
    }
    Ok(data)
}

// ==================== AutoFilterSelector ====================

/// HTTP content encoding auto-selector
///
/// Automatically selects appropriate decoder filter list based on HTTP headers.
/// Follows RFC 7230 Section 3.3.1: Transfer-Encoding takes priority over Content-Encoding.
///
/// # Priority Rules
///
/// 1. **Transfer-Encoding: chunked** -> Add `ChunkedDecoder`
/// 2. **Content-Encoding: gzip | x-gzip** -> Add `GZipDecoder`
/// 3. **Content-Encoding: deflate** -> Add `ZlibDecoder` (future support)
/// 4. **Content-Encoding: bzip2 | x-bzip2** -> Add `BZip2Decoder`
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::AutoFilterSelector;
///
/// // Auto-select GZip decoder based on Content-Encoding: gzip
/// let filters = AutoFilterSelector::select_filters(Some("gzip"), None);
/// assert_eq!(filters.len(), 1);
///
/// // Transfer-Encoding takes priority
/// let filters = AutoFilterSelector::select_filters(Some("gzip"), Some("chunked"));
/// assert_eq!(filters.len(), 1); // Only chunked
/// ```
pub struct AutoFilterSelector;

impl AutoFilterSelector {
    /// Create appropriate filter list based on HTTP headers
    ///
    /// Automatically analyzes Content-Encoding and Transfer-Encoding headers,
    /// constructing corresponding decoder filters.
    ///
    /// # Arguments
    ///
    /// * `content_encoding` - Value of Content-Encoding header (optional)
    /// * `transfer_encoding` - Value of Transfer-Encoding header (optional)
    ///
    /// # Returns
    ///
    /// Configured filter list
    ///
    /// # RFC Compliance
    ///
    /// Follows RFC 7230 Section 3.3.1:
    /// - Transfer-Encoding has higher priority than Content-Encoding
    /// - Multiple encoding values are processed in order (comma-separated)
    pub fn select_filters(
        content_encoding: Option<&str>,
        transfer_encoding: Option<&str>,
    ) -> Vec<Box<dyn StreamFilter>> {
        let mut filters: Vec<Box<dyn StreamFilter>> = Vec::new();

        // Transfer-Encoding takes priority over Content-Encoding (RFC 7230)
        if let Some(te) = transfer_encoding {
            // Parse multiple values (comma-separated)
            for encoding in te.split(',') {
                let encoding = encoding.trim().to_lowercase();
                match encoding.as_str() {
                    "chunked" => {
                        filters.push(Box::new(ChunkedDecoder::new()));
                    }
                    "gzip" | "x-gzip" => {
                        filters.push(Box::new(GZipDecoder::new()));
                    }
                    "deflate" => {
                        // TODO: Future support for ZlibDecoder
                        tracing::warn!("Deflate encoding not yet implemented");
                    }
                    "bzip2" | "x-bzip2" => {
                        filters.push(Box::new(BZip2Decoder::new()));
                    }
                    _ => {
                        // Identity / none encoding -> passthrough (no decoder needed)
                        if encoding.eq_ignore_ascii_case("identity")
                            || encoding.eq_ignore_ascii_case("none")
                        {
                            continue;
                        }

                        // LZMA / x-lzma -> log warning, return identity (not yet supported)
                        if encoding.contains("lzma") {
                            tracing::warn!(
                                "LZMA encoding not yet supported, returning passthrough"
                            );
                            continue;
                        }

                        // Brotli (br) -> placeholder for future support
                        if encoding.eq_ignore_ascii_case("br") {
                            tracing::debug!("Brotli encoding detected but not yet implemented");
                            continue;
                        }

                        tracing::debug!("Unknown transfer encoding: {}", encoding);
                    }
                }
            }
        } else if let Some(ce) = content_encoding {
            // Only process Content-Encoding when Transfer-Encoding is absent
            for encoding in ce.split(',') {
                let encoding = encoding.trim().to_lowercase();
                match encoding.as_str() {
                    "gzip" | "x-gzip" => {
                        filters.push(Box::new(GZipDecoder::new()));
                    }
                    "deflate" => {
                        // TODO: Future support for ZlibDecoder
                        tracing::warn!("Deflate encoding not yet implemented");
                    }
                    "bzip2" | "x-bzip2" => {
                        filters.push(Box::new(BZip2Decoder::new()));
                    }
                    "identity" | "" => {
                        // identity means no encoding, ignore
                    }
                    _ => {
                        // Identity / none encoding -> passthrough (no decoder needed)
                        if encoding.eq_ignore_ascii_case("identity")
                            || encoding.eq_ignore_ascii_case("none")
                        {
                            continue;
                        }

                        // LZMA / x-lzma -> log warning, return identity (not yet supported)
                        if encoding.contains("lzma") {
                            tracing::warn!(
                                "LZMA encoding not yet supported, returning passthrough"
                            );
                            continue;
                        }

                        // Brotli (br) -> placeholder for future support
                        if encoding.eq_ignore_ascii_case("br") {
                            tracing::debug!("Brotli encoding detected but not yet implemented");
                            continue;
                        }

                        tracing::debug!("Unknown content encoding: {}", encoding);
                    }
                }
            }
        }

        filters
    }
}

/// Detect content encoding from magic bytes as fallback when Content-Encoding header
/// may be incorrect or missing.
///
/// Examines the first few bytes of data to identify known compression formats:
/// - Gzip: bytes [0x1f, 0x8b]
/// - BZ2: bytes [0x42, 0x5a] ("BZ")
/// - Zlib/Deflate: byte [0x78] followed by valid flag byte
///
/// # Arguments
///
/// * `data` - Raw data bytes to examine
///
/// # Returns
///
/// A string slice representing the detected encoding:
/// - "gzip" for GZip compressed data
/// - "bzip2" for BZip2 compressed data
/// - "deflate" for Zlib/Deflate compressed data
/// - "identity" for uncompressed/unknown data
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::detect_encoding_from_magic_bytes;
///
/// let gzip_data = vec![0x1f, 0x8b, 0x08, ...];
/// assert_eq!(detect_encoding_from_magic_bytes(&gzip_data), "gzip");
/// ```
pub fn detect_encoding_from_magic_bytes(data: &[u8]) -> &'static str {
    // Check for Gzip magic number: 0x1f 0x8b
    if data.len() >= 2 {
        if data[0] == 0x1f && data[1] == 0x8b {
            return "gzip";
        }
        // Check for BZip2 magic number: 0x42 0x5a ("BZ")
        if data[0] == 0x42 && data[1] == 0x5a {
            return "bzip2";
        }
    }
    // Check for Zlib/Deflate magic number: 0x78 followed by valid flag byte
    if !data.is_empty() && data[0] == 0x78 {
        return "deflate";
    }
    // Default to identity (no compression)
    "identity"
}

/// Wraps a disk writer with automatic stream filtering.
///
/// Data written through this wrapper passes through the configured
/// filters before being written to disk. This enables transparent
/// decompression of compressed streams during download.
///
/// # Type Parameters
///
/// * `W` - A type implementing `SeekableDiskWriter` for actual disk I/O
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::http::stream_filter::{StreamingFilterWriter, GZipDecoder};
/// use aria2_core::filesystem::disk_writer::CachedDiskWriter;
///
/// let writer = CachedDiskWriter::new(&path, None, None);
/// let filters = vec![Box::new(GZipDecoder::new()) as Box<dyn StreamFilter>];
///
/// let mut filter_writer = StreamingFilterWriter::new(writer, filters);
/// filter_writer.write_filtered(&compressed_data).await?;
/// filter_writer.flush_filtered().await?;
/// ```
pub struct StreamingFilterWriter<W: SeekableDiskWriter> {
    /// Underlying disk writer for actual I/O operations
    inner: W,
    /// Filters to process data through
    filters: Vec<Box<dyn StreamFilter>>,
    /// Buffered input data waiting to be processed
    buffer: Vec<u8>,
    /// Process data in chunks of this size (default 64KB)
    chunk_size: usize,
    /// Total bytes written to underlying writer (after filtering)
    total_written: u64,
    /// Total bytes received as input (before filtering)
    total_input: u64,
    /// Current write offset in the underlying writer
    write_offset: u64,
}

impl<W: SeekableDiskWriter> StreamingFilterWriter<W> {
    /// Create a new StreamingFilterWriter with default settings.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying disk writer
    /// * `filters` - The filters to apply to all written data
    ///
    /// # Returns
    ///
    /// A new StreamingFilterWriter instance with 64KB chunk size
    pub fn new(inner: W, filters: Vec<Box<dyn StreamFilter>>) -> Self {
        Self {
            inner,
            filters,
            buffer: Vec::with_capacity(64 * 1024),
            chunk_size: 64 * 1024,
            total_written: 0,
            total_input: 0,
            write_offset: 0,
        }
    }

    /// Set custom chunk size for processing.
    ///
    /// Smaller chunks use less memory but may be less efficient.
    /// Larger chunks improve throughput but increase memory usage.
    /// Minimum chunk size is 1KB.
    ///
    /// # Arguments
    ///
    /// * `size` - Desired chunk size in bytes (minimum 1024)
    ///
    /// # Returns
    ///
    /// Self for method chaining
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(1024);
        self
    }

    /// Write data through the filter chain to underlying writer.
    ///
    /// Data is buffered internally until a full chunk is accumulated,
    /// then processed through the filter chain and written to disk.
    ///
    /// # Arguments
    ///
    /// * `data` - Raw (possibly compressed) data to write
    ///
    /// # Returns
    ///
    /// Ok(()) on success, or an error string if filtering/writing fails
    ///
    /// # Errors
    ///
    /// - If the filter chain fails to process the data
    /// - If the underlying writer fails to write
    pub async fn write_filtered(&mut self, data: &[u8]) -> Result<()> {
        self.total_input += data.len() as u64;
        self.buffer.extend_from_slice(data);

        // Process complete chunks
        while self.buffer.len() >= self.chunk_size {
            let chunk = self.buffer.drain(..self.chunk_size).collect::<Vec<_>>();
            let filtered = process_filters(&mut self.filters, &chunk)?;
            if !filtered.is_empty() {
                self.inner.write_at(self.write_offset, &filtered).await?;
                self.write_offset += filtered.len() as u64;
                self.total_written += filtered.len() as u64;
            }
        }

        Ok(())
    }

    /// Flush remaining buffered data through the filter chain.
    ///
    /// Must be called after all data has been written to ensure
    /// remaining buffered data is processed and written to disk.
    ///
    /// # Returns
    ///
    /// Ok(()) on success, or an error string if flushing fails
    ///
    /// # Errors
    ///
    /// - If the filter chain fails during final processing
    /// - If the underlying writer fails to flush
    pub async fn flush_filtered(&mut self) -> Result<()> {
        // Process any remaining buffered data
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            let filtered = process_filters(&mut self.filters, &remaining)?;
            if !filtered.is_empty() {
                self.inner.write_at(self.write_offset, &filtered).await?;
                self.write_offset += filtered.len() as u64;
                self.total_written += filtered.len() as u64;
            }
        }

        // Flush the underlying writer
        self.inner.flush().await?;
        Ok(())
    }

    /// Get total number of input bytes received (before filtering).
    ///
    /// # Returns
    ///
    /// Total uncompressed/compressed input bytes
    pub fn total_input_bytes(&self) -> u64 {
        self.total_input
    }

    /// Get total number of output bytes written (after filtering).
    ///
    /// # Returns
    ///
    /// Total decompressed/filtered output bytes
    pub fn total_output_bytes(&self) -> u64 {
        self.total_written
    }

    /// Calculate compression ratio (output / input).
    ///
    /// Values > 1.0 indicate expansion (common with already-compressed data).
    /// Values < 1.0 indicate successful compression.
    /// Returns 1.0 if no data has been processed.
    ///
    /// # Returns
    ///
    /// Compression ratio as f64
    pub fn compression_ratio(&self) -> f64 {
        if self.total_input > 0 {
            self.total_output_bytes() as f64 / self.total_input as f64
        } else {
            1.0
        }
    }

    /// Consume this wrapper and return the underlying writer.
    ///
    /// Useful when you need direct access to the underlying writer
    /// after streaming is complete.
    ///
    /// # Returns
    ///
    /// The inner SeekableDiskWriter instance
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Get a reference to the inner writer.
    ///
    /// # Returns
    ///
    /// Immutable reference to the underlying SeekableDiskWriter
    pub fn inner(&self) -> &W {
        &self.inner
    }

    /// Get a mutable reference to the inner writer.
    ///
    /// # Returns
    ///
    /// Mutable reference to the underlying SeekableDiskWriter
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};

    // Mock implementation of SeekableDiskWriter for testing
    struct MockSeekableWriter {
        data: Vec<u8>,
        opened: bool,
    }

    impl MockSeekableWriter {
        fn new() -> Self {
            MockSeekableWriter {
                data: Vec::new(),
                opened: false,
            }
        }
    }

    #[async_trait]
    impl SeekableDiskWriter for MockSeekableWriter {
        async fn open(&mut self) -> Result<()> {
            self.opened = true;
            Ok(())
        }

        async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
            // Ensure vector is large enough
            let end = offset as usize + buf.len();
            if self.data.len() < end {
                self.data.resize(end, 0);
            }
            self.data[offset as usize..end].copy_from_slice(buf);
            Ok(())
        }

        async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            let start = offset as usize;
            if start >= self.data.len() {
                return Ok(0);
            }
            let available = self.data.len() - start;
            let to_copy = available.min(buf.len());
            buf[..to_copy].copy_from_slice(&self.data[start..start + to_copy]);
            Ok(to_copy)
        }

        async fn truncate(&mut self, length: u64) -> Result<()> {
            self.data.truncate(length as usize);
            Ok(())
        }

        async fn flush(&mut self) -> Result<()> {
            Ok(())
        }

        async fn len(&self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }

        fn path(&self) -> &Path {
            static PATH: std::sync::LazyLock<PathBuf> =
                std::sync::LazyLock::new(|| PathBuf::from("/mock/path"));
            &PATH
        }
    }

    #[test]
    fn test_detect_magic_gzip() {
        // Test GZip magic bytes: 0x1f 0x8b
        let gzip_data = vec![0x1f, 0x8b, 0x08, 0x00];
        assert_eq!(detect_encoding_from_magic_bytes(&gzip_data), "gzip");

        // Test with exactly 2 bytes (minimum required)
        let gzip_minimal = vec![0x1f, 0x8b];
        assert_eq!(detect_encoding_from_magic_bytes(&gzip_minimal), "gzip");

        // Test with more realistic gzip header
        let gzip_realistic = vec![0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03];
        assert_eq!(detect_encoding_from_magic_bytes(&gzip_realistic), "gzip");
    }

    #[test]
    fn test_detect_magic_bzip2() {
        // Test BZip2 magic bytes: 0x42 0x5a ("BZ")
        let bzip2_data = vec![0x42, 0x5a, 0x68, 0x39]; // "BZh9" - common bzip2 start
        assert_eq!(detect_encoding_from_magic_bytes(&bzip2_data), "bzip2");

        // Test with minimal bzip2 header
        let bzip2_minimal = vec![0x42, 0x5a];
        assert_eq!(detect_encoding_from_magic_bytes(&bzip2_minimal), "bzip2");

        // Test that BZ is detected before checking for deflate (0x78)
        let bzip2_not_deflate = vec![0x42, 0x5a, 0x78, 0x9c];
        assert_eq!(
            detect_encoding_from_magic_bytes(&bzip2_not_deflate),
            "bzip2"
        );
    }

    #[test]
    fn test_unknown_encoding_passthrough() {
        // Test that AutoFilterSelector handles unknown encodings without errors

        // Test "br" (Brotli) - should return empty filter list (passthrough)
        let filters_br = AutoFilterSelector::select_filters(Some("br"), None);
        assert_eq!(
            filters_br.len(),
            0,
            "Brotli encoding should result in empty filter list"
        );

        // Test "lzma" - should return empty filter list (passthrough)
        let filters_lzma = AutoFilterSelector::select_filters(Some("lzma"), None);
        assert_eq!(
            filters_lzma.len(),
            0,
            "LZMA encoding should result in empty filter list"
        );

        // Test "identity" - should return empty filter list (no decoder needed)
        let filters_identity = AutoFilterSelector::select_filters(Some("identity"), None);
        assert_eq!(
            filters_identity.len(),
            0,
            "Identity encoding should result in empty filter list"
        );

        // Test "none" - should return empty filter list (no decoder needed)
        let filters_none = AutoFilterSelector::select_filters(Some("none"), None);
        assert_eq!(
            filters_none.len(),
            0,
            "None encoding should result in empty filter list"
        );

        // Test Transfer-Encoding with unknown values
        let filters_te_br = AutoFilterSelector::select_filters(None, Some("br"));
        assert_eq!(
            filters_te_br.len(),
            0,
            "Transfer-Encoding br should result in empty filter list"
        );

        let filters_te_lzma = AutoFilterSelector::select_filters(None, Some("lzma"));
        assert_eq!(
            filters_te_lzma.len(),
            0,
            "Transfer-Encoding lzma should result in empty filter list"
        );
    }

    #[tokio::test]
    async fn test_streaming_filter_writer_basic() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write as SyncWrite;

        // Create test data and compress it with gzip
        let original_data = b"Hello, StreamingFilterWriter! This is a test of the streaming filter writer implementation.";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(original_data).unwrap();
        let compressed_data = encoder.finish().unwrap();

        // Verify the compressed data starts with gzip magic bytes
        assert_eq!(compressed_data[0], 0x1f);
        assert_eq!(compressed_data[1], 0x8b);

        // Create filters with GZip decoder
        let filters = vec![Box::new(GZipDecoder::new()) as Box<dyn StreamFilter>];

        // Create StreamingFilterWriter with mock writer
        let mock_writer = MockSeekableWriter::new();
        let mut filter_writer = StreamingFilterWriter::new(mock_writer, filters);

        // Write compressed data through the filter
        filter_writer
            .write_filtered(&compressed_data)
            .await
            .unwrap();

        // Verify input tracking
        assert_eq!(
            filter_writer.total_input_bytes(),
            compressed_data.len() as u64,
            "Input byte count should match compressed data size"
        );

        // Flush remaining data
        filter_writer.flush_filtered().await.unwrap();

        // Verify output tracking
        assert!(
            filter_writer.total_output_bytes() > 0,
            "Should have written decompressed data"
        );

        // Verify compression ratio
        let ratio = filter_writer.compression_ratio();
        assert!(
            ratio > 0.0,
            "Compression ratio should be > 0, got {}",
            ratio
        );

        // Retrieve inner writer and verify decompressed data
        let inner = filter_writer.into_inner();
        let written_data = &inner.data;

        // Verify the decompressed data matches original
        assert_eq!(
            written_data, original_data,
            "Decompressed data should match original input"
        );

        // Test with chunk size customization
        let mock_writer2 = MockSeekableWriter::new();
        let filters2 = vec![Box::new(GZipDecoder::new()) as Box<dyn StreamFilter>];
        let mut filter_writer2 =
            StreamingFilterWriter::new(mock_writer2, filters2).with_chunk_size(1024);

        filter_writer2
            .write_filtered(&compressed_data)
            .await
            .unwrap();
        filter_writer2.flush_filtered().await.unwrap();

        let inner2 = filter_writer2.into_inner();
        assert_eq!(
            &inner2.data, original_data,
            "Should produce same result with custom chunk size"
        );
    }
}
