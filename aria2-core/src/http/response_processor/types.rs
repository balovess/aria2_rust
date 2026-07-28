//! Core types for the HTTP response processor.
//!
//! Contains the result enum, configuration struct, and their implementations.
//! These types define the public API contract between the response processor
//! and the download engine.

use crate::http::metalink_http::MetalinkHttpDigest;

// ---------------------------------------------------------------------------
// ResponseProcessResult
// ---------------------------------------------------------------------------

/// Outcome of processing an HTTP response header.
///
/// Each variant carries the information needed by the download engine to
/// determine what to do next -- start downloading, follow a redirect, retry
/// with credentials, etc.
#[derive(Debug)]
pub enum ResponseProcessResult {
    /// 2xx response -- the download is ready to proceed.
    ///
    /// The engine should create a download command and begin receiving body data.
    DownloadReady {
        /// Determined output filename (from Content-Disposition or URL path).
        filename: String,
        /// Total entity length in bytes. 0 means unknown (e.g. chunked + gzip).
        entity_length: u64,
        /// Content-Type header value, if present.
        content_type: Option<String>,
        /// Whether content-encoding is gzip/deflate (disables segmented download).
        inflate_required: bool,
        /// Whether the response uses chunked transfer-encoding.
        chunked: bool,
        /// Whether the total length is explicitly known (even if 0).
        knows_total_length: bool,
        /// Whether the server supports persistent (keep-alive) connections.
        supports_persistent_connection: bool,
        /// Whether the HTTP method should switch from HEAD to GET.
        switch_head_to_get: bool,
        /// Alternative URIs from Metalink/HTTP Link headers.
        metalink_uris: Vec<String>,
        /// Digests from Digest header for checksum verification.
        digests: Vec<MetalinkHttpDigest>,
        /// Parsed Content-Range (start, end, total) if present in 206 response.
        content_range: Option<(u64, u64, u64)>,
        /// Last-Modified header value as IMF-fixdate string, if present.
        /// Used by the `remote-time` option to set the local file's mtime.
        last_modified: Option<String>,
    },

    /// 304 Not Modified -- the file is already current; all pieces are done.
    NotModified {
        /// Entity length reported by the server.
        entity_length: u64,
    },

    /// 3xx redirect -- delegate to skip_response handler.
    Redirect(crate::http::skip_response::HttpRedirectInfo),

    /// 401/407 authentication challenge -- delegate to skip_response handler.
    AuthChallenge(crate::http::skip_response::HttpAuthChallenge),

    /// 4xx/5xx error (non-redirect, non-auth).
    Error {
        /// HTTP status code.
        status_code: u16,
        /// Human-readable error description.
        message: String,
    },

    /// The request needs to be retried (e.g. after HEAD -> GET switch
    /// with other-encoding path).
    RetryNeeded,
}

// ---------------------------------------------------------------------------
// ResponseProcessorConfig
// ---------------------------------------------------------------------------

/// Configuration for the HTTP response processor.
///
/// Collects the options that affect response processing behavior,
/// mirroring the C++ options accessed via `getOption()`.
#[derive(Debug, Clone)]
pub struct ResponseProcessorConfig {
    /// Whether to derive filename from Content-Disposition header.
    pub content_disposition_default_utf8: bool,
    /// Whether the original request accepted gzip encoding (Accept-Encoding: gzip).
    pub accept_gzip: bool,
    /// Directory for output files.
    pub output_dir: String,
    /// Whether to use remote Last-Modified time for the local file.
    pub remote_time: bool,
    /// Whether this is a dry run (don't actually download).
    pub dry_run: bool,
    /// Maximum pipelined requests (only relevant if persistent connection).
    pub max_pipelined_request: u32,
    /// Preferred geographic locations for Metalink/HTTP mirror selection.
    /// Matches C++ `PREF_METALINK_LOCATION`: comma-separated location strings
    /// that boost the priority of matching mirrors.
    pub metalink_location: Vec<String>,
}

impl Default for ResponseProcessorConfig {
    fn default() -> Self {
        Self {
            content_disposition_default_utf8: false,
            accept_gzip: true,
            output_dir: String::new(),
            remote_time: false,
            dry_run: false,
            max_pipelined_request: 1,
            metalink_location: Vec::new(),
        }
    }
}
