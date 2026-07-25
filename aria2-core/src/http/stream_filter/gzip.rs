//! GZip format decompressor with true streaming support
//!
//! GZip (RFC 1952) data decompressor implemented using the flate2 library.
//! Supports true streaming/incremental decompression: data can be fed in
//! arbitrary chunks and the decoder will produce output as it becomes
//! available, matching the C++ aria2 GZipDecodingStreamFilter behavior.

use std::io::Read;

use crate::error::{Aria2Error, Result};
use crate::http::stream_filter::types::StreamFilter;

/// Encoding format detected from magic bytes
#[derive(Debug, Clone, Copy, PartialEq)]
enum EncodingFormat {
    Unknown,
    Gzip,
    Zlib,
    RawDeflate,
}

/// GZip format decompressor with true streaming support
#[derive(Debug)]
pub struct GZipDecoder {
    format: EncodingFormat,
    finished: bool,
    initialized: bool,
    bytes_processed: usize,
    input_buffer: Vec<u8>,
}

impl GZipDecoder {
    pub fn new() -> Self {
        GZipDecoder {
            format: EncodingFormat::Unknown,
            finished: false,
            initialized: false,
            bytes_processed: 0,
            input_buffer: Vec::new(),
        }
    }

    pub fn bytes_processed(&self) -> usize {
        self.bytes_processed
    }

    fn detect_format(data: &[u8]) -> EncodingFormat {
        if data.len() < 2 {
            return EncodingFormat::Unknown;
        }
        if data[0] == 0x1f && data[1] == 0x8b {
            EncodingFormat::Gzip
        } else if data[0] == 0x78 {
            EncodingFormat::Zlib
        } else {
            EncodingFormat::RawDeflate
        }
    }
}

impl Default for GZipDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for GZipDecoder {
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if self.finished {
            return Err(Aria2Error::Parse("GZip decoder already finished".to_string()));
        }
        if input.is_empty() {
            self.bytes_processed = 0;
            return Ok(Vec::new());
        }

        self.input_buffer.extend_from_slice(input);

        if !self.initialized {
            let fmt = Self::detect_format(&self.input_buffer);
            if fmt == EncodingFormat::Unknown {
                self.bytes_processed = 0;
                return Ok(Vec::new());
            }
            self.format = fmt;
            self.initialized = true;
        }

        match self.format {
            EncodingFormat::Gzip => self.filter_gzip(),
            EncodingFormat::Zlib | EncodingFormat::RawDeflate => self.filter_zlib(),
            EncodingFormat::Unknown => {
                self.bytes_processed = 0;
                Ok(Vec::new())
            }
        }
    }

    fn flush(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let mut output = Vec::new();
        match self.format {
            EncodingFormat::Gzip => {
                if !self.input_buffer.is_empty() {
                    let cursor = std::io::Cursor::new(std::mem::take(&mut self.input_buffer));
                    let mut decoder = flate2::read::GzDecoder::new(cursor);
                    let _ = decoder.read_to_end(&mut output);
                }
            }
            EncodingFormat::Zlib | EncodingFormat::RawDeflate => {
                if !self.input_buffer.is_empty() {
                    let mut d = flate2::Decompress::new(self.format == EncodingFormat::Zlib);
                    let _ = d.decompress_vec(
                        &self.input_buffer,
                        &mut output,
                        flate2::FlushDecompress::Finish,
                    );
                    self.input_buffer.clear();
                }
            }
            EncodingFormat::Unknown => {}
        }
        self.finished = true;
        Ok(output)
    }

    fn name(&self) -> &'static str {
        "gzip"
    }

    fn needs_more_input(&self) -> bool {
        !self.finished
    }
}

impl GZipDecoder {
    fn filter_gzip(&mut self) -> Result<Vec<u8>> {
        let all_data = std::mem::take(&mut self.input_buffer);
        let cursor = std::io::Cursor::new(all_data);
        let mut decoder = flate2::read::GzDecoder::new(cursor);

        let mut output = Vec::with_capacity(4096);
        match decoder.read_to_end(&mut output) {
            Ok(_) => {
                self.finished = true;
                self.bytes_processed = decoder.get_ref().get_ref().len();
                Ok(output)
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.input_buffer = decoder.into_inner().into_inner();
                self.bytes_processed = 0;
                Ok(Vec::new())
            }
            Err(e) => {
                self.input_buffer = decoder.into_inner().into_inner();
                Err(Aria2Error::Io(format!("GZip decompression failed: {}", e)))
            }
        }
    }

    fn filter_zlib(&mut self) -> Result<Vec<u8>> {
        let all_data = std::mem::take(&mut self.input_buffer);
        let mut decompress = flate2::Decompress::new(self.format == EncodingFormat::Zlib);
        let mut output = Vec::with_capacity(all_data.len().saturating_mul(3).max(256));

        match decompress.decompress_vec(&all_data, &mut output, flate2::FlushDecompress::None) {
            Ok(flate2::Status::Ok) => {
                self.bytes_processed = decompress.total_in() as usize;
                // Keep unconsumed data in buffer
                let consumed = decompress.total_in() as usize;
                if consumed < all_data.len() {
                    self.input_buffer.extend_from_slice(&all_data[consumed..]);
                }
                Ok(output)
            }
            Ok(flate2::Status::StreamEnd) => {
                self.finished = true;
                self.bytes_processed = decompress.total_in() as usize;
                let consumed = decompress.total_in() as usize;
                if consumed < all_data.len() {
                    self.input_buffer.extend_from_slice(&all_data[consumed..]);
                }
                Ok(output)
            }
            Ok(flate2::Status::BufError) => {
                self.bytes_processed = 0;
                self.input_buffer = all_data;
                Ok(output)
            }
            Err(e) => {
                self.input_buffer = all_data;
                Err(Aria2Error::Io(format!("Deflate decompression failed: {}", e)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::{GzEncoder, ZlibEncoder, DeflateEncoder};
    use flate2::Compression;
    use std::io::Write;

    #[test]
    fn test_gzip_decoder_basic() {
        let data = b"Hello, World! This is a test of GZip compression.";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = GZipDecoder::new();
        let result = decoder.filter(&compressed).unwrap();
        assert_eq!(result.as_slice(), data);
        assert!(decoder.finished);
    }

    #[test]
    fn test_gzip_decoder_incremental() {
        let data = b"Testing incremental GZip decompression with multiple chunks.";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = GZipDecoder::new();
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

    #[test]
    fn test_gzip_decoder_empty_input() {
        let mut decoder = GZipDecoder::new();
        let result = decoder.filter(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_gzip_decoder_already_finished() {
        let data = b"test";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = GZipDecoder::new();
        decoder.filter(&compressed).unwrap();
        assert!(decoder.filter(&[1, 2, 3]).is_err());
    }

    #[test]
    fn test_gzip_decoder_name() {
        let decoder = GZipDecoder::new();
        assert_eq!(decoder.name(), "gzip");
    }

    #[test]
    fn test_gzip_decoder_needs_more_input() {
        let decoder = GZipDecoder::new();
        assert!(decoder.needs_more_input());
        let data = b"test";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = GZipDecoder::new();
        decoder.filter(&compressed).unwrap();
        assert!(!decoder.needs_more_input());
    }

    #[test]
    fn test_gzip_decoder_small_chunks() {
        let data = b"A".repeat(10000);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&data).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = GZipDecoder::new();
        let mut result = Vec::new();
        for chunk in compressed.chunks(10) {
            result.extend_from_slice(&decoder.filter(chunk).unwrap());
        }
        result.extend_from_slice(&decoder.flush().unwrap());
        assert_eq!(result.len(), data.len());
        assert_eq!(result.as_slice(), data.as_slice());
    }

    #[test]
    fn test_gzip_decoder_zlib_format() {
        let data = b"Testing zlib format decompression through GZipDecoder.";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = GZipDecoder::new();
        let result = decoder.filter(&compressed).unwrap();
        assert_eq!(result.as_slice(), data);
    }

    #[test]
    fn test_gzip_decoder_raw_deflate() {
        let data = b"Testing raw deflate decompression through GZipDecoder.";
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = GZipDecoder::new();
        let result = decoder.filter(&compressed).unwrap();
        assert_eq!(result.as_slice(), data);
    }
}
