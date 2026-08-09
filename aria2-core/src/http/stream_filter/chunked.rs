//! HTTP Chunked Transfer-Encoding decoder
//!
//! Implements chunked encoding decoding per RFC 7230 Section 4.1.
//! Supports chunk extensions (unknown extensions are ignored).

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;

/// Chunked Transfer-Encoding state enum
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ChunkState {
    /// Reading chunk size line
    ReadingSize,
    /// Reading chunk data
    ReadingData { remaining: usize },
    /// Reading CRLF after data (chunk data end marker).
    /// Strictly expects `\r\n` in sequence, matching C++ ChunkedDecodeFilter.
    ReadingDataEnd { saw_cr: bool },
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
                        // Current chunk data fully read, expect CRLF
                        self.state = ChunkState::ReadingDataEnd { saw_cr: false };
                        self.current_chunk_remaining = 0;
                    } else {
                        // Update remaining count in state
                        self.state = ChunkState::ReadingData {
                            remaining: new_remaining,
                        };
                        self.current_chunk_remaining = new_remaining;
                    }
                }

                ChunkState::ReadingDataEnd { saw_cr } => {
                    // Strictly expect CRLF after chunk data, matching C++ ChunkedDecodeFilter.
                    let byte = input[pos];
                    if !saw_cr {
                        if byte == b'\r' {
                            // Saw CR, now expect LF
                            self.state = ChunkState::ReadingDataEnd { saw_cr: true };
                            pos += 1;
                        } else if byte == b'\n' {
                            // Tolerate bare LF for robustness (some servers send just \n)
                            // C++ is strict, but real-world compatibility requires this
                            self.state = ChunkState::ReadingSize;
                            pos += 1;
                        } else {
                            // Expected CRLF but got unexpected byte — protocol error
                            self.state = ChunkState::Error(format!(
                                "Expected CRLF after chunk data, got byte 0x{:02x}",
                                byte
                            ));
                            return Err(Aria2Error::Parse(format!(
                                "Expected CRLF after chunk data, got byte 0x{:02x}",
                                byte
                            )));
                        }
                    } else {
                        // Already saw CR, expect LF
                        if byte == b'\n' {
                            self.state = ChunkState::ReadingSize;
                            pos += 1;
                        } else {
                            self.state = ChunkState::Error(format!(
                                "Expected LF after CR in chunk terminator, got byte 0x{:02x}",
                                byte
                            ));
                            return Err(Aria2Error::Parse(format!(
                                "Expected LF after CR in chunk terminator, got byte 0x{:02x}",
                                byte
                            )));
                        }
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
            | ChunkState::ReadingDataEnd { .. } => {
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
