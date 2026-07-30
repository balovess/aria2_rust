//! Streaming HTTP header processor
//!
//! Incrementally parses HTTP response headers as they arrive over the network.
//! This is the Rust equivalent of the C++ `HttpHeaderProcessor`, providing:
//! - Streaming/incremental parsing (data may arrive in arbitrary TCP chunks)
//! - Obsolete line folding (obs-fold, RFC 7230 section 3.2.4)
//! - Case-insensitive header name lookup
//! - Transfer-Encoding overrides Content-Length per RFC 7230 section 3.3.2
//!
//! # Example
//!
//! ```rust,ignore
//! use aria2_core::http::header_processor::HttpHeaderProcessor;
//!
//! let mut proc = HttpHeaderProcessor::new();
//!
//! // First chunk arrives (incomplete)
//! let state = proc.feed(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n");
//! assert!(!state.is_complete());
//!
//! // Second chunk with end-of-headers marker
//! let state = proc.feed(b"Content-Length: 42\r\n\r\n");
//! assert!(state.is_complete());
//!
//! let head = proc.get_result().unwrap();
//! assert_eq!(head.status_code, 200);
//! ```

mod processor;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public items so they remain accessible via header_processor::
pub use processor::HttpHeaderProcessor;
pub use types::{HttpHeaderParseState, HttpResponseHead};
