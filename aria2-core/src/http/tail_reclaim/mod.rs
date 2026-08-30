//! HTTP tail reclaim policy for stalled download recovery.
//!
//! When an HTTP download connection stalls (no data received for a configurable
//! timeout), this module calculates whether the tail portion of the connection's
//! assigned range can be reclaimed and reassigned to a new connection.
//!
//! This matches the C++ aria2 behavior where `HttpRequest::tailRequestEnabled_`
//! and related logic detects stalled connections and splits the remaining range
//! to allow parallel download completion.
//!
//! # Tail Reclaim Algorithm
//!
//! 1. Track bytes received per connection over time
//! 2. When a connection has not received data for `stall_timeout` seconds:
//!    a. Calculate the remaining unrequested bytes in the connection's range
//!    b. If remaining > `min_tail_length`, split the tail off
//!    c. The original connection keeps its in-flight requests
//!    d. A new connection can pick up the tail portion
//! 3. The tail length is calculated as:
//!    `remaining = end - (start + bytes_received + bytes_in_flight)`
//!    If remaining > min_tail_length, the tail starts at
//!    `start + bytes_received + bytes_in_flight`
//!
//! # Relationship to engine::http_tail_reclaim
//!
//! The `engine::http_tail_reclaim` module makes the *global* decision of whether
//! the download as a whole should reclaim its HTTP tail segment (considering
//! protocol, p2p involvement, concurrent command counts, etc.).
//!
//! This module operates at the *per-connection* level: it tracks whether an
//! individual connection has stalled and computes the exact byte range to
//! split off as the tail. The two modules are complementary — the engine-level
//! policy decides *when* to consider reclaiming, while this module decides
//! *what* to reclaim from a specific connection.

mod tracker;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public items for backward compatibility so that external code
// can still use `aria2_core::http::tail_reclaim::TailReclaimConfig` etc.
pub use tracker::ConnectionStallTracker;
pub use types::{
    DEFAULT_MIN_TAIL_LENGTH, DEFAULT_STALL_TIMEOUT_SECS, DEFAULT_TAIL_RECLAIM_ENABLED,
    TailReclaimConfig, TailReclaimConnectionState, TailReclaimResult,
};
