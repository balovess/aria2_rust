//! Metalink/HTTP parser (RFC 6249 / RFC 5988 / RFC 3230)
//!
//! Parses `Link` headers (RFC 5988) and `Digest` headers (RFC 3230) from HTTP
//! responses to extract alternative download URLs and content verification
//! digests, matching the C++ `MetalinkHttpEntry` and `HttpResponse` parsing
//! logic from the original aria2.
//!
//! # Link header format (RFC 5988)
//!
//! ```text
//! Link: <http://mirror1>; rel="duplicate"; pri="1",
//!       <http://mirror2>; rel="duplicate"; pri="2"
//! ```
//!
//! # Digest header format (RFC 3230)
//!
//! ```text
//! Digest: sha-256=base64value,md5=base64value
//! ```

mod helpers;
mod parser;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types so that external consumers can access them
// at the same paths as before (e.g., `metalink_http::MetalinkHttpParser`).
pub use types::{MetalinkHttpDigest, MetalinkHttpLink, MetalinkHttpResult, DEFAULT_PRI, MAX_PRI};
pub use parser::MetalinkHttpParser;
