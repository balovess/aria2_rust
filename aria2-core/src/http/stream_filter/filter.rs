//! Filter processing helpers and auto-selector
//!
//! Provides composable stream data filter processing functions and
//! automatic filter selection based on HTTP headers.

use crate::error::Result;
use crate::http::stream_filter::bzip2::BZip2Decoder;
use crate::http::stream_filter::chunked::ChunkedDecoder;
use crate::http::stream_filter::gzip::GZipDecoder;
use crate::http::stream_filter::types::StreamFilter;

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
