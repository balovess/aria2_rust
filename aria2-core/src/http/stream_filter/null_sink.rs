//! Null sink stream filter
//!
//! Discards all input data, used for skipping HTTP response bodies
//! (redirects, error responses, etc.) where the content needs to be
//! consumed from the stream but not stored.
//!
//! Based on C++ aria2's NullSinkStreamFilter / SinkStreamFilter which
//! discards data while still passing it through the filter chain.

use crate::error::Result;
use crate::http::stream_filter::types::StreamFilter;

/// Null sink filter that discards all input data
///
/// This filter consumes input data but returns empty output.
/// It's used when HTTP response bodies need to be consumed
/// (e.g., for redirects, 4xx/5xx responses) but the data
/// should not be written to disk.
///
/// In the C++ implementation, this is equivalent to NullSinkStreamFilter
/// which is chained after ChunkedDecodingStreamFilter to properly
/// consume chunked framing while discarding the actual content.
#[derive(Debug)]
pub struct NullSinkFilter {
    /// Total bytes consumed (for tracking progress)
    total_consumed: u64,
    /// Whether the filter has been completed
    finished: bool,
}

impl NullSinkFilter {
    /// Create a new null sink filter
    pub fn new() -> Self {
        NullSinkFilter {
            total_consumed: 0,
            finished: false,
        }
    }

    /// Get total bytes consumed
    pub fn total_consumed(&self) -> u64 {
        self.total_consumed
    }

    /// Mark the filter as finished
    pub fn finish(&mut self) {
        self.finished = true;
    }
}

impl Default for NullSinkFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter for NullSinkFilter {
    /// Consume input data and return empty output
    ///
    /// All input bytes are counted but the data is discarded.
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.total_consumed += input.len() as u64;
        Ok(Vec::new())
    }

    /// Flush returns nothing since all data was discarded
    fn flush(&mut self) -> Result<Vec<u8>> {
        self.finished = true;
        Ok(Vec::new())
    }

    /// Returns "null_sink"
    fn name(&self) -> &'static str {
        "null_sink"
    }

    /// Null sink always accepts more input until finished
    fn needs_more_input(&self) -> bool {
        !self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null_sink_new() {
        let filter = NullSinkFilter::new();
        assert_eq!(filter.total_consumed(), 0);
        assert!(!filter.finished);
    }

    #[test]
    fn test_null_sink_default() {
        let filter = NullSinkFilter::default();
        assert_eq!(filter.total_consumed(), 0);
    }

    #[test]
    fn test_null_sink_consumes_data() {
        let mut filter = NullSinkFilter::new();
        let data = b"Hello, World!";
        let result = filter.filter(data).unwrap();
        assert!(result.is_empty());
        assert_eq!(filter.total_consumed(), 13);
    }

    #[test]
    fn test_null_sink_multiple_calls() {
        let mut filter = NullSinkFilter::new();
        filter.filter(b"first").unwrap();
        filter.filter(b"second").unwrap();
        filter.filter(b"third").unwrap();
        assert_eq!(filter.total_consumed(), 16); // 5 + 6 + 5
    }

    #[test]
    fn test_null_sink_empty_input() {
        let mut filter = NullSinkFilter::new();
        let result = filter.filter(&[]).unwrap();
        assert!(result.is_empty());
        assert_eq!(filter.total_consumed(), 0);
    }

    #[test]
    fn test_null_sink_flush() {
        let mut filter = NullSinkFilter::new();
        filter.filter(b"some data").unwrap();
        let result = filter.flush().unwrap();
        assert!(result.is_empty());
        assert!(filter.finished);
    }

    #[test]
    fn test_null_sink_finished_ignores_input() {
        let mut filter = NullSinkFilter::new();
        filter.finish();
        let result = filter.filter(b"ignored").unwrap();
        assert!(result.is_empty());
        assert_eq!(filter.total_consumed(), 0); // Nothing counted after finish
    }

    #[test]
    fn test_null_sink_name() {
        let filter = NullSinkFilter::new();
        assert_eq!(filter.name(), "null_sink");
    }

    #[test]
    fn test_null_sink_needs_more_input() {
        let mut filter = NullSinkFilter::new();
        assert!(filter.needs_more_input());
        filter.finish();
        assert!(!filter.needs_more_input());
    }
}
