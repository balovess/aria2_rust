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
        let mut filters: Vec<Box<dyn