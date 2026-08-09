//! Data types and constants for Metalink/HTTP parsing (RFC 6249 / RFC 5988 / RFC 3230).

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default priority when no `pri` parameter is given (matches C++ aria2).
pub const DEFAULT_PRI: u64 = 999999;
/// Maximum allowed priority value (matches C++ aria2).
pub const MAX_PRI: u64 = 999999;

// ---------------------------------------------------------------------------
// MetalinkHttpLink
// ---------------------------------------------------------------------------

/// A single link extracted from a `Link` header (RFC 5988).
///
/// Represents an alternative download URL with associated metadata.
/// Only links with `rel="duplicate"` or `rel="mirror"` are considered
/// relevant for Metalink/HTTP purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetalinkHttpLink {
    /// The download URL from the link target.
    pub uri: String,
    /// Relationship types (e.g., "duplicate", "mirror").
    pub rel: Vec<String>,
    /// Priority — lower values are preferred. `None` means lowest priority.
    pub pri: Option<u64>,
    /// Whether this link is preferred (has the `pref` bare parameter).
    pub pref: bool,
    /// Content type (from `type` parameter).
    pub type_: Option<String>,
    /// Language tag (from `hreflang` parameter).
    pub lang: Option<String>,
    /// Geographic location (from `geo` parameter), lowercased.
    pub geo: Option<String>,
}

impl MetalinkHttpLink {
    pub(crate) fn new(uri: String) -> Self {
        Self {
            uri,
            rel: Vec::new(),
            pri: None,
            pref: false,
            type_: None,
            lang: None,
            geo: None,
        }
    }

    /// Returns the effective sort key: pref links come first, then by pri ascending.
    pub fn sort_key(&self) -> (bool, u64) {
        (!self.pref, self.pri.unwrap_or(DEFAULT_PRI))
    }

    /// Whether this link is relevant for Metalink/HTTP (has "duplicate" or "mirror" rel).
    pub fn is_relevant(&self) -> bool {
        self.rel.iter().any(|r| {
            let r_lower = r.to_lowercase();
            r_lower == "duplicate" || r_lower == "mirror"
        })
    }
}

// ---------------------------------------------------------------------------
// MetalinkHttpDigest
// ---------------------------------------------------------------------------

/// A content digest extracted from a `Digest` header (RFC 3230).
///
/// Per RFC 3230, digest values are base64-encoded. Some implementations
/// use hex encoding; the consumer must handle decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetalinkHttpDigest {
    /// Algorithm name (lowercased), e.g. "sha-256", "sha-512", "md5".
    pub algorithm: String,
    /// Raw digest value (may be base64 or hex encoded).
    pub value: String,
}

// ---------------------------------------------------------------------------
// MetalinkHttpResult
// ---------------------------------------------------------------------------

/// Combined result of parsing `Link` and `Digest` headers from an HTTP response.
#[derive(Debug, Clone, Default)]
pub struct MetalinkHttpResult {
    /// Alternative download URLs, sorted by priority (pref first, then pri ascending).
    pub links: Vec<MetalinkHttpLink>,
    /// Content verification digests.
    pub digests: Vec<MetalinkHttpDigest>,
}
