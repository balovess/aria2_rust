//! DNS cache entry type.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

/// A single cached DNS entry containing resolved addresses and metadata.
///
/// Each entry stores the resolved socket addresses for a hostname,
/// along with when it was resolved and its time-to-live duration.
#[derive(Debug, Clone)]
pub struct DnsEntry {
    /// The hostname this entry was resolved for
    pub hostname: String,
    /// Resolved socket addresses (sorted by preference)
    pub addresses: Vec<SocketAddr>,
    /// Timestamp when this entry was created/resolved
    pub resolved_at: Instant,
    /// Time-to-live for this entry before it's considered stale
    pub ttl: Duration,
    /// Whether IPv4 addresses should be preferred in ordering
    pub ipv4_preferred: bool,
}

impl DnsEntry {
    /// Check if this DNS entry has expired based on its TTL.
    ///
    /// Returns `true` if the elapsed time since resolution exceeds the TTL,
    /// meaning the entry should be re-resolved.
    pub fn is_expired(&self) -> bool {
        self.resolved_at.elapsed() > self.ttl
    }

    /// Get the best address from this entry.
    ///
    /// If IPv4 is preferred, returns the first IPv4 address if available,
    /// otherwise falls back to the first address in the list.
    /// Returns `None` if there are no addresses.
    pub fn best_address(&self) -> Option<SocketAddr> {
        if self.addresses.is_empty() {
            return None;
        }
        if self.ipv4_preferred {
            self.addresses
                .iter()
                .find(|a| matches!(a.ip(), IpAddr::V4(_)))
                .copied()
                .or_else(|| self.addresses.first().copied())
        } else {
            Some(self.addresses[0])
        }
    }

    /// Return a clone of all cached addresses for this entry.
    pub fn all_addresses(&self) -> Vec<SocketAddr> {
        self.addresses.clone()
    }
}
