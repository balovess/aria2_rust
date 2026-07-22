//! Per-connection request state object.
//!
//! Equivalent to the C++ aria2 `Request` class. Tracks the URI (original and
//! current-after-redirect), HTTP method, connection info, retry/redirect
//! counters, keep-alive/pipelining hints, peer statistics, and wake-time
//! backoff state for a single download connection.
//!
//! # Key Invariants
//!
//! - `uri` is the **original** URI set via `set_uri()` and never changes
//!   after being set (even across redirects).
//! - `current_uri` reflects the **current** URI which may differ from `uri`
//!   after HTTP redirects. Fragments are always stripped.
//! - `parsed_url` is derived from `current_uri` via the `url` crate and is
//!   kept in sync with it.
//!
//! # Thread Safety
//!
//! `Request` is **not** `Sync` — it is meant to be owned by a single
//! connection/task. If sharing is needed, wrap in `Arc<Mutex<Request>>`.

use std::time::Instant;
use url::Url;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// HTTP GET method string.
pub const METHOD_GET: &str = "GET";

/// HTTP HEAD method string.
pub const METHOD_HEAD: &str = "HEAD";

/// Maximum number of HTTP redirects allowed.
pub const MAX_REDIRECT: u32 = 20;

/// Default filename when the URI path ends with `/`.
pub const DEFAULT_FILE: &str = "index.html";

// ---------------------------------------------------------------------------
// PeerStat — lightweight per-connection download statistics
// ---------------------------------------------------------------------------

/// Per-connection download speed tracker for a single peer/server.
///
/// Created from the **original** URI's host and protocol (not the redirected
/// one), because the URI selector picks mirrors based on the original URI.
#[derive(Debug, Clone)]
pub struct PeerStat {
    /// Connection identifier (0 until assigned by the engine).
    pub cuid: u64,
    /// Current instantaneous download speed (bytes/sec).
    pub download_speed: u64,
    /// Exponential moving average download speed (bytes/sec).
    pub avg_download_speed: u64,
    /// Bytes downloaded in the current session.
    pub session_download_length: u64,
    /// Server hostname (from original URI).
    pub hostname: String,
    /// Protocol scheme, e.g. "http", "https", "ftp" (from original URI).
    pub protocol: String,
}

impl PeerStat {
    /// Create a new `PeerStat` for the given host and protocol.
    pub fn new(cuid: u64, hostname: String, protocol: String) -> Self {
        Self {
            cuid,
            download_speed: 0,
            avg_download_speed: 0,
            session_download_length: 0,
            hostname,
            protocol,
        }
    }

    /// Add `length` bytes to the session download counter (saturating).
    pub fn add_session_download_length(&mut self, length: u64) {
        self.session_download_length = self.session_download_length.saturating_add(length);
    }

    /// Return the average download speed (bytes/sec).
    pub fn avg_download_speed(&self) -> u64 {
        self.avg_download_speed
    }
}

// ---------------------------------------------------------------------------
// Request — per-connection request state
// ---------------------------------------------------------------------------

/// Per-connection request state.
///
/// Tracks the URI lifecycle (original → redirect chain), retry/redirect
/// counters, connection hints, and peer statistics for one download
/// connection. See module-level documentation for key invariants.
#[derive(Debug, Clone)]
pub struct Request {
    // ── Parsed URI components (cached, like C++ UriStruct) ──────────────
    /// Parsed representation of `current_uri`. Always kept in sync.
    parsed_url: Url,
    /// Cached host from `parsed_url`. For IPv6, stored WITHOUT brackets
    /// (e.g. "::1"), matching C++ `us_.host`.
    host: String,
    /// Cached protocol/scheme from `parsed_url` (e.g. "http", "https").
    protocol: String,
    /// Cached port from `parsed_url` (explicit or scheme default).
    port: u16,
    /// Whether the host is an IPv6 literal address.
    ipv6_literal_address: bool,

    // ── URI strings ──────────────────────────────────────────────────────
    /// Original URI as passed to `set_uri()`. Never changes after set.
    uri: String,
    /// Current URI (may differ from `uri` after redirects). Fragment stripped.
    current_uri: String,
    /// URI used as the Referer header. Fragment stripped.
    referer: String,

    // ── HTTP method ──────────────────────────────────────────────────────
    /// HTTP request method (GET or HEAD).
    method: String,

    // ── Connected address info ───────────────────────────────────────────
    /// Hostname of the actual connected server.
    connected_hostname: String,
    /// IP address of the actual connected server.
    connected_addr: String,
    /// Port of the actual connected server.
    connected_port: u16,

    // ── Counters ─────────────────────────────────────────────────────────
    /// Retry attempt count for this URI.
    try_count: u32,
    /// HTTP redirect count.
    redirect_count: u32,

    // ── Connection hints ─────────────────────────────────────────────────
    /// Server supports persistent (keep-alive) connections.
    supports_persistent_connection: bool,
    /// User/config hint to enable keep-alive.
    keep_alive_hint: bool,
    /// User/config hint to enable pipelining.
    pipelining_hint: bool,
    /// Maximum number of pipelined requests.
    max_pipelined_request: u32,

    // ── Peer statistics ──────────────────────────────────────────────────
    /// Download speed/stats tracker for this peer. `None` until `init_peer_stat()`.
    peer_stat: Option<PeerStat>,

    // ── Removal flag ─────────────────────────────────────────────────────
    /// Flag to request this `Request` be removed from pools.
    removal_requested: bool,

    // ── Wake time (backoff) ──────────────────────────────────────────────
    /// Time after which this request can be retried.
    wake_time: Instant,
    /// If true, reset `try_count` when wake time expires (aria2-next feature).
    reset_try_count_after_wake: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Strip the fragment (`#…`) portion from a URI string.
fn remove_fragment(uri: &str) -> &str {
    match uri.find('#') {
        Some(pos) => &uri[..pos],
        None => uri,
    }
}

/// Check if the string up to `://` looks like a valid URI scheme.
///
/// Per RFC 3986 §3.1: `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn is_absolute_uri(uri: &str) -> bool {
    match uri.find("://") {
        Some(scheme_end) if scheme_end > 0 => uri[..scheme_end]
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c.is_ascii_digit() || c == '+' || c == '-' || c == '.'),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl Request {
    /// Create a new `Request` with the given URI.
    ///
    /// Parses the URI, strips the fragment, and sets `current_uri`. Returns
    /// `None` if the URI cannot be parsed.
    pub fn new(uri: &str) -> Option<Self> {
        let mut req = Self::default_inner();
        req.set_uri(uri).then_some(req)
    }

    /// Set the original URI and parse it.
    ///
    /// Sets `uri` to the given string, parses it into `current_uri` and
    /// `parsed_url` (fragment stripped), and resets
    /// `supports_persistent_connection` to `true`.
    ///
    /// Returns `true` on success, `false` if the URI cannot be parsed.
    pub fn set_uri(&mut self, uri: &str) -> bool {
        self.supports_persistent_connection = true;
        self.uri = uri.to_owned();
        self.parse_uri(uri)
    }

    /// Handle an HTTP redirect to `uri`.
    ///
    /// Increments `redirect_count`, resolves relative and protocol-relative
    /// URIs against the current URI, parses the result (fragment stripped),
    /// and resets `supports_persistent_connection` to `true`.
    ///
    /// **Does NOT alter `uri`** (the original URI is preserved).
    ///
    /// Returns `true` on success, `false` if the redirect URI is empty or
    /// cannot be parsed.
    pub fn redirect_uri(&mut self, uri: &str) -> bool {
        self.supports_persistent_connection = true;
        self.redirect_count = self.redirect_count.saturating_add(1);

        if uri.is_empty() {
            return false;
        }

        let redirected = if uri.starts_with("//") {
            // Protocol-relative URI (RFC 3986 §4.2): e.g. "//host/path"
            format!("{}:{}", self.protocol(), uri)
        } else if is_absolute_uri(uri) {
            // Absolute URI: e.g. "http://host/path"
            uri.to_owned()
        } else {
            // Relative URI: resolve against current URI.
            match Url::parse(&self.current_uri) {
                Ok(base) => base.join(uri).map(|u| u.to_string()).unwrap_or_else(|_| uri.to_owned()),
                Err(_) => uri.to_owned(),
            }
        };

        self.parse_uri(&redirected)
    }

    /// Re-parse the original URI (`uri`), resetting `current_uri` and
    /// `parsed_url` back to the initial state.
    ///
    /// Resets `supports_persistent_connection` to `true` and clears
    /// connected address info.
    ///
    /// Returns `true` on success, `false` if the original URI cannot be
    /// re-parsed (should not happen under normal operation).
    pub fn reset_uri(&mut self) -> bool {
        self.supports_persistent_connection = true;
        self.connected_hostname.clear();
        self.connected_addr.clear();
        self.connected_port = 0;
        let uri = self.uri.clone();
        self.parse_uri(&uri)
    }

    /// Internal: parse a URI string (strip fragment → parse → update fields).
    ///
    /// Returns `true` on success.
    fn parse_uri(&mut self, src_uri: &str) -> bool {
        let without_fragment = remove_fragment(src_uri);
        match Url::parse(without_fragment) {
            Ok(url) => {
                // Cache URI components from the parsed URL, matching C++ UriStruct.
                let ipv6_literal = matches!(url.host(), Some(url::Host::Ipv6(_)));
                // For IPv6, host_str() returns "[::1]" with brackets;
                // strip them to match C++ us_.host which stores "::1".
                let host = match url.host_str() {
                    Some(h) if ipv6_literal => {
                        // Strip surrounding brackets: "[::1]" → "::1"
                        h.strip_prefix('[')
                            .and_then(|s| s.strip_suffix(']'))
                            .unwrap_or(h)
                            .to_owned()
                    }
                    Some(h) => h.to_owned(),
                    None => String::new(),
                };
                let protocol = url.scheme().to_owned();
                let port = url.port_or_known_default().unwrap_or(0);

                self.current_uri = without_fragment.to_owned();
                self.parsed_url = url;
                self.host = host;
                self.protocol = protocol;
                self.port = port;
                self.ipv6_literal_address = ipv6_literal;
                true
            }
            Err(e) => {
                tracing::warn!("Failed to parse URI '{}': {}", without_fragment, e);
                false
            }
        }
    }

    // ── Try count ────────────────────────────────────────────────────────

    /// Reset the retry counter to zero.
    pub fn reset_try_count(&mut self) {
        self.try_count = 0;
    }

    /// Increment the retry counter by one.
    pub fn add_try_count(&mut self) {
        self.try_count = self.try_count.saturating_add(1);
    }

    /// Return the current retry count.
    pub fn try_count(&self) -> u32 {
        self.try_count
    }

    // ── Redirect count ───────────────────────────────────────────────────

    /// Reset the redirect counter to zero.
    pub fn reset_redirect_count(&mut self) {
        self.redirect_count = 0;
    }

    /// Return the current redirect count.
    pub fn redirect_count(&self) -> u32 {
        self.redirect_count
    }

    // ── URI accessors ────────────────────────────────────────────────────

    /// Return the original URI (as passed to `set_uri`).
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Return the current URI (may differ after redirects, fragment stripped).
    pub fn current_uri(&self) -> &str {
        &self.current_uri
    }

    /// Return the referer URI (fragment stripped).
    pub fn referer(&self) -> &str {
        &self.referer
    }

    /// Set the referer, stripping the fragment before storing.
    pub fn set_referer(&mut self, uri: &str) {
        self.referer = remove_fragment(uri).to_owned();
    }

    // ── URI component accessors (derived from parsed_url) ────────────────

    /// Return the scheme/protocol (e.g. "http", "https", "ftp").
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// Return the host portion of the URI.
    ///
    /// For `http://example.com/path`, returns `"example.com"`.
    /// For `http://[::1]/path`, returns `"::1"` (without brackets),
    /// matching the C++ `getHost()` behavior.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Return the host, wrapping IPv6 literal addresses in square brackets.
    ///
    /// For `"::1"` returns `"[::1]"`; for `"example.com"` returns
    /// `"example.com"`. Matches C++ `getURIHost()`.
    pub fn uri_host(&self) -> String {
        if self.ipv6_literal_address {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// Return the port number. Falls back to the scheme's default port
    /// if not explicitly specified in the URI.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Return the directory portion of the path (everything before the
    /// last `/`).
    ///
    /// For `/dir/subdir/file.txt`, returns `/dir/subdir/`.
    pub fn dir(&self) -> &str {
        let path = self.parsed_url.path();
        match path.rfind('/') {
            Some(pos) => &path[..=pos],
            None => "/",
        }
    }

    /// Return the file portion of the path (everything after the last `/`).
    ///
    /// For `/dir/subdir/file.txt`, returns `file.txt`.
    /// For `/dir/subdir/`, returns `DEFAULT_FILE` ("index.html").
    pub fn file(&self) -> &str {
        let path = self.parsed_url.path();
        match path.rfind('/') {
            Some(pos) if pos + 1 < path.len() => &path[pos + 1..],
            _ => DEFAULT_FILE,
        }
    }

    /// Return the query string (without the leading `?`), or empty if absent.
    pub fn query(&self) -> &str {
        self.parsed_url.query().unwrap_or("")
    }

    /// Return the username embedded in the URI, or empty if absent.
    pub fn username(&self) -> &str {
        self.parsed_url.username()
    }

    /// Return the password embedded in the URI, or empty if absent.
    pub fn password(&self) -> Option<&str> {
        self.parsed_url.password()
    }

    /// Return `true` if the current URI has an embedded password.
    pub fn has_password(&self) -> bool {
        self.parsed_url.password().is_some()
    }

    /// Return `true` if the host is an IPv6 literal address.
    pub fn is_ipv6_literal_address(&self) -> bool {
        self.ipv6_literal_address
    }

    // ── HTTP method ──────────────────────────────────────────────────────

    /// Set the HTTP method (typically GET or HEAD).
    pub fn set_method(&mut self, method: &str) {
        self.method = method.to_owned();
    }

    /// Return the HTTP method.
    pub fn method(&self) -> &str {
        &self.method
    }

    // ── Persistent connection / keep-alive / pipelining ──────────────────

    /// Set whether the server supports persistent connections.
    pub fn set_supports_persistent_connection(&mut self, f: bool) {
        self.supports_persistent_connection = f;
    }

    /// Return whether the server supports persistent connections.
    pub fn supports_persistent_connection(&self) -> bool {
        self.supports_persistent_connection
    }

    /// Return whether keep-alive is effectively enabled.
    ///
    /// True only when both the server supports persistent connections **and**
    /// the keep-alive hint is set.
    pub fn is_keep_alive_enabled(&self) -> bool {
        self.supports_persistent_connection && self.keep_alive_hint
    }

    /// Set the keep-alive hint.
    pub fn set_keep_alive_hint(&mut self, hint: bool) {
        self.keep_alive_hint = hint;
    }

    /// Return whether pipelining is effectively enabled.
    ///
    /// True only when both the server supports persistent connections **and**
    /// the pipelining hint is set.
    pub fn is_pipelining_enabled(&self) -> bool {
        self.supports_persistent_connection && self.pipelining_hint
    }

    /// Set the pipelining hint.
    pub fn set_pipelining_hint(&mut self, hint: bool) {
        self.pipelining_hint = hint;
    }

    /// Return the raw pipelining hint (without checking persistent connection).
    pub fn is_pipelining_hint(&self) -> bool {
        self.pipelining_hint
    }

    /// Set the maximum number of pipelined requests.
    pub fn set_max_pipelined_request(&mut self, num: u32) {
        self.max_pipelined_request = num;
    }

    /// Return the maximum number of pipelined requests.
    pub fn max_pipelined_request(&self) -> u32 {
        self.max_pipelined_request
    }

    // ── Peer statistics ──────────────────────────────────────────────────

    /// Return a reference to the current `PeerStat`, if initialized.
    pub fn peer_stat(&self) -> Option<&PeerStat> {
        self.peer_stat.as_ref()
    }

    /// Initialize `PeerStat` from the **original** URI's host and protocol.
    ///
    /// Uses the original URI (not the redirected one) because the URI
    /// selector selects mirrors based on the original URI. Replaces any
    /// existing `PeerStat`.
    pub fn init_peer_stat(&mut self) -> &PeerStat {
        // Parse the original URI to extract host and protocol.
        // Under normal operation this should always succeed (it was
        // successfully parsed by set_uri), but we fall back gracefully.
        let (host, protocol) = match Url::parse(remove_fragment(&self.uri)) {
            Ok(url) => (
                url.host_str().unwrap_or("").to_owned(),
                url.scheme().to_owned(),
            ),
            Err(_) => (String::new(), String::new()),
        };
        self.peer_stat = Some(PeerStat::new(0, host, protocol));
        // Safe: we just set it to Some above.
        self.peer_stat.as_ref().unwrap()
    }

    // ── Removal ──────────────────────────────────────────────────────────

    /// Request that this `Request` be removed from pools.
    pub fn request_removal(&mut self) {
        self.removal_requested = true;
    }

    /// Return whether removal has been requested.
    pub fn removal_requested(&self) -> bool {
        self.removal_requested
    }

    // ── Connected address info ───────────────────────────────────────────

    /// Set the actual connected server's hostname, IP address, and port.
    pub fn set_connected_addr_info(&mut self, hostname: &str, addr: &str, port: u16) {
        self.connected_hostname = hostname.to_owned();
        self.connected_addr = addr.to_owned();
        self.connected_port = port;
    }

    /// Return the connected server's hostname.
    pub fn connected_hostname(&self) -> &str {
        &self.connected_hostname
    }

    /// Return the connected server's IP address.
    pub fn connected_addr(&self) -> &str {
        &self.connected_addr
    }

    /// Return the connected server's port.
    pub fn connected_port(&self) -> u16 {
        self.connected_port
    }

    // ── Wake time / backoff ──────────────────────────────────────────────

    /// Set the wake time (earliest time this request can be retried).
    pub fn set_wake_time(&mut self, time: Instant) {
        self.wake_time = time;
    }

    /// Return the wake time.
    pub fn wake_time(&self) -> Instant {
        self.wake_time
    }

    /// Return whether it is time to wake (i.e., now >= wake_time).
    pub fn is_wake_time_reached(&self) -> bool {
        Instant::now() >= self.wake_time
    }

    // ── resetTryCountAfterWake (aria2-next feature) ──────────────────────

    /// Set whether `try_count` should be reset when the wake time expires.
    pub fn set_reset_try_count_after_wake(&mut self, f: bool) {
        self.reset_try_count_after_wake = f;
    }

    /// Return whether `try_count` should be reset when the wake time expires.
    pub fn reset_try_count_after_wake(&self) -> bool {
        self.reset_try_count_after_wake
    }

    // ── Default constructor (private) ────────────────────────────────────

    /// Construct a `Request` with all fields at their default values.
    ///
    /// Note: `parsed_url` is initialized to a dummy URL; it must be
    /// overwritten by `set_uri()` before use.
    fn default_inner() -> Self {
        Self {
            parsed_url: Url::parse("http://localhost/").unwrap(),
            host: String::new(),
            protocol: String::new(),
            port: 0,
            ipv6_literal_address: false,
            uri: String::new(),
            current_uri: String::new(),
            referer: String::new(),
            method: METHOD_GET.to_owned(),
            connected_hostname: String::new(),
            connected_addr: String::new(),
            connected_port: 0,
            try_count: 0,
            redirect_count: 0,
            supports_persistent_connection: true,
            keep_alive_hint: false,
            pipelining_hint: false,
            max_pipelined_request: 1,
            peer_stat: None,
            removal_requested: false,
            wake_time: Instant::now(),
            reset_try_count_after_wake: false,
        }
    }
}

impl Default for Request {
    /// Create a default `Request` (no URI set, method = GET).
    ///
    /// **Note:** A default-constructed `Request` has no valid URI. You must
    /// call `set_uri()` before using URI-dependent accessors.
    fn default() -> Self {
        Self::default_inner()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── Construction ─────────────────────────────────────────────────────

    #[test]
    fn test_new_valid_uri() {
        let req = Request::new("http://example.com/path/file.zip").unwrap();
        assert_eq!(req.uri(), "http://example.com/path/file.zip");
        assert_eq!(req.current_uri(), "http://example.com/path/file.zip");
        assert_eq!(req.protocol(), "http");
        assert_eq!(req.host(), "example.com");
        assert_eq!(req.port(), 80);
        assert_eq!(req.file(), "file.zip");
        assert_eq!(req.dir(), "/path/");
        assert_eq!(req.method(), "GET");
    }

    #[test]
    fn test_new_invalid_uri() {
        assert!(Request::new("not a url :///").is_none());
    }

    #[test]
    fn test_default() {
        let req = Request::default();
        assert_eq!(req.method(), "GET");
        assert_eq!(req.try_count(), 0);
        assert_eq!(req.redirect_count(), 0);
        assert!(req.supports_persistent_connection());
        assert!(!req.keep_alive_hint);
        assert!(!req.pipelining_hint);
        assert_eq!(req.max_pipelined_request(), 1);
        assert!(!req.removal_requested());
        assert_eq!(req.connected_port(), 0);
    }

    // ── Fragment stripping ───────────────────────────────────────────────

    #[test]
    fn test_fragment_stripped_on_set_uri() {
        let req = Request::new("http://example.com/file#section").unwrap();
        assert_eq!(req.current_uri(), "http://example.com/file");
        // uri() preserves the original including the fragment
        assert_eq!(req.uri(), "http://example.com/file#section");
    }

    #[test]
    fn test_fragment_stripped_on_referer() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_referer("http://example.com/page#anchor");
        assert_eq!(req.referer(), "http://example.com/page");
    }

    // ── Redirect handling ────────────────────────────────────────────────

    #[test]
    fn test_redirect_absolute_uri() {
        let mut req = Request::new("http://example.com/old").unwrap();
        assert!(req.redirect_uri("http://other.com/new"));
        assert_eq!(req.current_uri(), "http://other.com/new");
        // Original URI unchanged
        assert_eq!(req.uri(), "http://example.com/old");
        assert_eq!(req.redirect_count(), 1);
    }

    #[test]
    fn test_redirect_relative_uri() {
        let mut req = Request::new("http://example.com/dir/old").unwrap();
        assert!(req.redirect_uri("newfile"));
        assert_eq!(req.current_uri(), "http://example.com/dir/newfile");
        assert_eq!(req.redirect_count(), 1);
    }

    #[test]
    fn test_redirect_protocol_relative_uri() {
        let mut req = Request::new("http://example.com/old").unwrap();
        assert!(req.redirect_uri("//other.com/new"));
        assert_eq!(req.current_uri(), "http://other.com/new");
        assert_eq!(req.redirect_count(), 1);
    }

    #[test]
    fn test_redirect_protocol_relative_preserves_https() {
        let mut req = Request::new("https://example.com/old").unwrap();
        assert!(req.redirect_uri("//other.com/new"));
        assert_eq!(req.current_uri(), "https://other.com/new");
    }

    #[test]
    fn test_redirect_empty_uri_fails() {
        let mut req = Request::new("http://example.com/old").unwrap();
        assert!(!req.redirect_uri(""));
        // redirect_count still incremented (matches C++ behavior)
        assert_eq!(req.redirect_count(), 1);
    }

    #[test]
    fn test_redirect_fragment_stripped() {
        let mut req = Request::new("http://example.com/old").unwrap();
        assert!(req.redirect_uri("http://other.com/new#section"));
        assert_eq!(req.current_uri(), "http://other.com/new");
    }

    #[test]
    fn test_redirect_resets_persistent_connection() {
        let mut req = Request::new("http://example.com/old").unwrap();
        req.set_supports_persistent_connection(false);
        assert!(req.redirect_uri("http://other.com/new"));
        assert!(req.supports_persistent_connection());
    }

    // ── Reset URI ────────────────────────────────────────────────────────

    #[test]
    fn test_reset_uri() {
        let mut req = Request::new("http://example.com/original").unwrap();
        req.redirect_uri("http://other.com/redirected");
        assert_eq!(req.current_uri(), "http://other.com/redirected");

        assert!(req.reset_uri());
        assert_eq!(req.current_uri(), "http://example.com/original");
        assert_eq!(req.uri(), "http://example.com/original");
    }

    #[test]
    fn test_reset_uri_clears_connected_addr() {
        let mut req = Request::new("http://example.com/file").unwrap();
        req.set_connected_addr_info("example.com", "93.184.216.34", 80);
        assert!(req.reset_uri());
        assert_eq!(req.connected_hostname(), "");
        assert_eq!(req.connected_addr(), "");
        assert_eq!(req.connected_port(), 0);
    }

    // ── Try count ────────────────────────────────────────────────────────

    #[test]
    fn test_try_count() {
        let mut req = Request::new("http://example.com/").unwrap();
        assert_eq!(req.try_count(), 0);
        req.add_try_count();
        req.add_try_count();
        assert_eq!(req.try_count(), 2);
        req.reset_try_count();
        assert_eq!(req.try_count(), 0);
    }

    // ── Redirect count ───────────────────────────────────────────────────

    #[test]
    fn test_redirect_count() {
        let mut req = Request::new("http://example.com/").unwrap();
        assert_eq!(req.redirect_count(), 0);
        req.redirect_uri("http://a.com/");
        req.redirect_uri("http://b.com/");
        assert_eq!(req.redirect_count(), 2);
        req.reset_redirect_count();
        assert_eq!(req.redirect_count(), 0);
    }

    // ── Wake time / resetTryCountAfterWake ───────────────────────────────

    #[test]
    fn test_wake_time() {
        let mut req = Request::new("http://example.com/").unwrap();
        let future = Instant::now() + Duration::from_secs(60);
        req.set_wake_time(future);
        assert_eq!(req.wake_time(), future);
        assert!(!req.is_wake_time_reached());
    }

    #[test]
    fn test_wake_time_already_passed() {
        let mut req = Request::new("http://example.com/").unwrap();
        let past = Instant::now() - Duration::from_secs(1);
        req.set_wake_time(past);
        assert!(req.is_wake_time_reached());
    }

    #[test]
    fn test_reset_try_count_after_wake() {
        let mut req = Request::new("http://example.com/").unwrap();
        assert!(!req.reset_try_count_after_wake());
        req.set_reset_try_count_after_wake(true);
        assert!(req.reset_try_count_after_wake());
    }

    // ── Keep-alive and pipelining ────────────────────────────────────────

    #[test]
    fn test_keep_alive_disabled_by_default() {
        let req = Request::new("http://example.com/").unwrap();
        assert!(!req.is_keep_alive_enabled());
    }

    #[test]
    fn test_keep_alive_enabled_with_hint() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_keep_alive_hint(true);
        assert!(req.is_keep_alive_enabled());
    }

    #[test]
    fn test_keep_alive_disabled_when_no_persistent() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_keep_alive_hint(true);
        req.set_supports_persistent_connection(false);
        assert!(!req.is_keep_alive_enabled());
    }

    #[test]
    fn test_pipelining_disabled_by_default() {
        let req = Request::new("http://example.com/").unwrap();
        assert!(!req.is_pipelining_enabled());
    }

    #[test]
    fn test_pipelining_enabled_with_hint() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_pipelining_hint(true);
        assert!(req.is_pipelining_enabled());
    }

    #[test]
    fn test_pipelining_disabled_when_no_persistent() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_pipelining_hint(true);
        req.set_supports_persistent_connection(false);
        assert!(!req.is_pipelining_enabled());
    }

    #[test]
    fn test_pipelining_hint_raw() {
        let mut req = Request::new("http://example.com/").unwrap();
        assert!(!req.is_pipelining_hint());
        req.set_pipelining_hint(true);
        assert!(req.is_pipelining_hint());
    }

    #[test]
    fn test_max_pipelined_request() {
        let mut req = Request::new("http://example.com/").unwrap();
        assert_eq!(req.max_pipelined_request(), 1);
        req.set_max_pipelined_request(5);
        assert_eq!(req.max_pipelined_request(), 5);
    }

    // ── PeerStat ─────────────────────────────────────────────────────────

    #[test]
    fn test_peer_stat_none_initially() {
        let req = Request::new("http://example.com/").unwrap();
        assert!(req.peer_stat().is_none());
    }

    #[test]
    fn test_init_peer_stat() {
        let mut req = Request::new("http://example.com/file").unwrap();
        let stat = req.init_peer_stat();
        assert_eq!(stat.cuid, 0);
        assert_eq!(stat.hostname, "example.com");
        assert_eq!(stat.protocol, "http");
    }

    #[test]
    fn test_init_peer_stat_uses_original_uri() {
        let mut req = Request::new("http://original.com/file").unwrap();
        req.redirect_uri("https://redirected.com/other");
        let stat = req.init_peer_stat();
        // PeerStat uses original URI, not redirected
        assert_eq!(stat.hostname, "original.com");
        assert_eq!(stat.protocol, "http");
    }

    // ── Removal ──────────────────────────────────────────────────────────

    #[test]
    fn test_removal_flag() {
        let mut req = Request::new("http://example.com/").unwrap();
        assert!(!req.removal_requested());
        req.request_removal();
        assert!(req.removal_requested());
    }

    // ── Connected address info ───────────────────────────────────────────

    #[test]
    fn test_connected_addr_info() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_connected_addr_info("cdn.example.com", "93.184.216.34", 443);
        assert_eq!(req.connected_hostname(), "cdn.example.com");
        assert_eq!(req.connected_addr(), "93.184.216.34");
        assert_eq!(req.connected_port(), 443);
    }

    // ── URI component accessors ──────────────────────────────────────────

    #[test]
    fn test_protocol_and_host() {
        let req = Request::new("https://www.example.com/path").unwrap();
        assert_eq!(req.protocol(), "https");
        assert_eq!(req.host(), "www.example.com");
    }

    #[test]
    fn test_port_explicit() {
        let req = Request::new("http://example.com:8080/path").unwrap();
        assert_eq!(req.port(), 8080);
    }

    #[test]
    fn test_port_default_http() {
        let req = Request::new("http://example.com/path").unwrap();
        assert_eq!(req.port(), 80);
    }

    #[test]
    fn test_port_default_https() {
        let req = Request::new("https://example.com/path").unwrap();
        assert_eq!(req.port(), 443);
    }

    #[test]
    fn test_dir_and_file() {
        let req = Request::new("http://example.com/dir/subdir/file.txt").unwrap();
        assert_eq!(req.dir(), "/dir/subdir/");
        assert_eq!(req.file(), "file.txt");
    }

    #[test]
    fn test_file_default_for_trailing_slash() {
        let req = Request::new("http://example.com/dir/").unwrap();
        assert_eq!(req.file(), DEFAULT_FILE);
    }

    #[test]
    fn test_file_default_for_root() {
        let req = Request::new("http://example.com/").unwrap();
        assert_eq!(req.file(), DEFAULT_FILE);
    }

    #[test]
    fn test_query() {
        let req = Request::new("http://example.com/file?key=value&foo=bar").unwrap();
        assert_eq!(req.query(), "key=value&foo=bar");
    }

    #[test]
    fn test_query_empty() {
        let req = Request::new("http://example.com/file").unwrap();
        assert_eq!(req.query(), "");
    }

    #[test]
    fn test_username_and_password() {
        let req = Request::new("http://user:pass@example.com/file").unwrap();
        assert_eq!(req.username(), "user");
        assert_eq!(req.password(), Some("pass"));
        assert!(req.has_password());
    }

    #[test]
    fn test_no_password() {
        let req = Request::new("http://user@example.com/file").unwrap();
        assert_eq!(req.username(), "user");
        assert!(req.password().is_none());
        assert!(!req.has_password());
    }

    #[test]
    fn test_ipv6_literal_address() {
        let req = Request::new("http://[::1]/path").unwrap();
        assert!(req.is_ipv6_literal_address());
        assert_eq!(req.host(), "::1");
        assert_eq!(req.uri_host(), "[::1]");
    }

    #[test]
    fn test_ipv4_not_ipv6_literal() {
        let req = Request::new("http://example.com/path").unwrap();
        assert!(!req.is_ipv6_literal_address());
        assert_eq!(req.uri_host(), "example.com");
    }

    // ── Method ───────────────────────────────────────────────────────────

    #[test]
    fn test_method_get_default() {
        let req = Request::new("http://example.com/").unwrap();
        assert_eq!(req.method(), METHOD_GET);
    }

    #[test]
    fn test_method_head() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_method(METHOD_HEAD);
        assert_eq!(req.method(), METHOD_HEAD);
    }

    // ── Constants ────────────────────────────────────────────────────────

    #[test]
    fn test_max_redirect_constant() {
        assert_eq!(MAX_REDIRECT, 20);
    }

    #[test]
    fn test_default_file_constant() {
        assert_eq!(DEFAULT_FILE, "index.html");
    }

    // ── set_uri resets persistent connection ─────────────────────────────

    #[test]
    fn test_set_uri_resets_persistent_connection() {
        let mut req = Request::new("http://example.com/").unwrap();
        req.set_supports_persistent_connection(false);
        assert!(req.set_uri("http://other.com/"));
        assert!(req.supports_persistent_connection());
    }

    // ── PeerStat standalone ──────────────────────────────────────────────

    #[test]
    fn test_peer_stat_standalone() {
        let mut stat = PeerStat::new(42, "example.com".into(), "http".into());
        assert_eq!(stat.cuid, 42);
        assert_eq!(stat.hostname, "example.com");
        assert_eq!(stat.protocol, "http");
        assert_eq!(stat.avg_download_speed(), 0);

        stat.add_session_download_length(1024);
        assert_eq!(stat.session_download_length, 1024);
        stat.add_session_download_length(u64::MAX);
        // Saturating add — should not overflow
        assert_eq!(stat.session_download_length, u64::MAX);
    }
}
