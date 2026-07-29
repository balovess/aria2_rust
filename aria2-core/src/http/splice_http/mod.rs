//! Linux-only zero-copy HTTP download via `splice(2)`.
//!
//! When downloading a file over plain HTTP (no HTTPS, no proxy) on Linux,
//! this module bypasses the hyper/reqwest HTTP client and uses a raw TCP
//! connection to enable `splice(2)` zero-copy transfer from socket to file.
//!
//! The response headers are read into user space (small, ~1 KB), then the
//! response body is spliced directly from the kernel socket buffer to the
//! output file via a pipe buffer — no user-space data copy for the body.
//!
//! # Limitations
//! - Linux only (splice is a Linux-specific syscall)
//! - Plain HTTP only (no TLS/HTTPS support)
//! - No proxy support
//! - No custom headers or cookies (use the reqwest path for those)
//! - HTTP 1.1 only (no HTTP/2)
//! - Requires `206 Partial Content` response (Range request)
//! - No chunked transfer encoding (Content-Length required)

mod download;
mod helpers;

#[cfg(test)]
mod tests;

pub use download::try_splice_download;
