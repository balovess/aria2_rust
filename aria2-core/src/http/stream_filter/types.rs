//! Stream filter trait definition
//!
//! Defines the interface for stream data processors. All concrete filter implementations
//! must implement the `StreamFilter` trait.

use crate::error::Result;

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
