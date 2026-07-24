//! Stream data decoder framework
//!
//! Provides composable stream data filters, supporting decoding of GZip, Deflate,
//! Chunked, BZip2 and other encoding formats. Multiple filters can be chained via
//! process_filters to implement complex data processing pipelines.
//!
//! Also provides NullSinkFilter for discarding response bodies (used when skipping
//! HTTP error/redirect responses).

pub mod bzip2;
pub mod chunked;
pub mod deflate;
pub mod filter;
pub mod gzip;
pub mod null_sink;
pub mod types;

// Re-export public API for convenient access via crate::http::stream_filter::*
pub use bzip2::BZip2Decoder;
pub use chunked::ChunkedDecoder;
pub use deflate::DeflateDecoder;
pub use filter::{detect_encoding_from_magic_bytes, flush_filters, process_filters, AutoFilterSelector};
pub use gzip::GZipDecoder;
pub use null_sink::NullSinkFilter;
pub use types::StreamFilter;
