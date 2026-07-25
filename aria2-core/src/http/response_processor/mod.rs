//! HTTP 2xx success response processor
//!
//! Processes HTTP responses after header parsing and determines the next action
//! for the download engine. This is the Rust equivalent of C++ aria2's
//! `HttpResponseCommand::executeInternal()`, covering:
//!
//! - 304 Not Modified handling (mark all pieces done)
//! - Metalink/HTTP Link header processing (alternate URIs)
//! - Digest header processing (checksum verification)
//! - Range validation (Content-Range vs requested range)
//! - Filename determination (Content-Disposition / URL path)
//! - Content-encoding detection (gzip/deflate disables segmented download)
//! - Zero-length file handling
//! - HEAD -> GET method switching
//! - Connection persistence (keep-alive) detection
//! - Unique protocol URI cleanup
//!
//! # Design
//!
//! Unlike the C++ command pattern which creates new command objects and adds them
//! to the download engine, this module returns structured `ResponseProcessResult`
//! values. The caller (download engine / connection handler) interprets the
//! result and takes appropriate action. This is more Rust-idiomatic and avoids
//! the complexity of the C++ command allocation lifecycle.
//!
//! # Module layout
//!
//! - [`types`]      -- `ResponseProcessResult` and `ResponseProcessorConfig`
//! - [`processor`]   -- `HttpResponseProcessor` (main entry point)
//! - [`connection`]  -- Keep-alive and content-encoding helpers
//! - [`filename`]    -- Filename determination from Content-Disposition / URL
//! - [`range`]       -- Range validation and Content-Range parsing
//! - [`conversion`]  -- HttpResponseHead-to-HttpResponse bridge

pub mod connection;
pub mod conversion;
pub mod filename;
pub mod processor;
pub mod range;
pub mod types;

#[cfg(test)]
mod tests;

// Re-export public API so external consumers can use
// `response_processor::HttpResponseProcessor` etc. without knowing the
// internal file layout.
pub use connection::{should_inflate_content_encoding, supports_persistent_connection};
pub use filename::determine_filename;
pub use processor::HttpResponseProcessor;
pub use range::validate_response_range;
pub use types::{ResponseProcessResult, ResponseProcessorConfig};
