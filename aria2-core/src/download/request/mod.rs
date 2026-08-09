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

pub mod peer_stat;
pub mod request_impl;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Re-exports — preserve the original public API
// ---------------------------------------------------------------------------

pub use peer_stat::PeerStat;
pub use request_impl::{
    DEFAULT_FILE, MAX_REDIRECT, METHOD_GET, METHOD_HEAD, is_absolute_uri, remove_fragment,
};

// ---------------------------------------------------------------------------
// Request — per-connection request state (struct definition only)
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
    pub(super) parsed_url: Url,
    /// Cached host from `parsed_url`. For IPv6, stored WITHOUT brackets
    /// (e.g. "::1"), matching C++ `us_.host`.
    pub(super) host: String,
    /// Cached protocol/scheme from `parsed_url` (e.g. "http", "https").
    pub(super) protocol: String,
    /// Cached port from `parsed_url` (explicit or scheme default).
    pub(super) port: u16,
    /// Whether the host is an IPv6 literal address.
    pub(super) ipv6_literal_address: bool,

    // ── URI strings ──────────────────────────────────────────────────────
    /// Original URI as passed to `set_uri()`. Never changes after set.
    pub(super) uri: String,
    /// Current URI (may differ from `uri` after redirects). Fragment stripped.
    pub(super) current_uri: String,
    /// URI used as the Referer header. Fragment stripped.
    pub(super) referer: String,

    // ── HTTP method ──────────────────────────────────────────────────────
    /// HTTP request method (GET or HEAD).
    pub(super) method: String,

    // ── Connected address info ───────────────────────────────────────────
    /// Hostname of the actual connected server.
    pub(super) connected_hostname: String,
    /// IP address of the actual connected server.
    pub(super) connected_addr: String,
    /// Port of the actual connected server.
    pub(super) connected_port: u16,

    // ── Counters ─────────────────────────────────────────────────────────
    /// Retry attempt count for this URI.
    pub(super) try_count: u32,
    /// HTTP redirect count.
    pub(super) redirect_count: u32,

    // ── Connection hints ─────────────────────────────────────────────────
    /// Server supports persistent (keep-alive) connections.
    pub(super) supports_persistent_connection: bool,
    /// User/config hint to enable keep-alive.
    pub(super) keep_alive_hint: bool,
    /// User/config hint to enable pipelining.
    pub(super) pipelining_hint: bool,
    /// Maximum number of pipelined requests.
    pub(super) max_pipelined_request: u32,

    // ── Peer statistics ──────────────────────────────────────────────────
    /// Download speed/stats tracker for this peer. `None` until `init_peer_stat()`.
    pub(super) peer_stat: Option<PeerStat>,

    // ── Removal flag ─────────────────────────────────────────────────────
    /// Flag to request this `Request` be removed from pools.
    pub(super) removal_requested: bool,

    // ── Wake time (backoff) ──────────────────────────────────────────────
    /// Time after which this request can be retried.
    pub(super) wake_time: Instant,
    /// If true, reset `try_count` when wake time expires (aria2-next feature).
    pub(super) reset_try_count_after_wake: bool,
}
