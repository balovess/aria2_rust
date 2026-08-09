//! DNS cache entry type.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::network::EndpointKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateAddress {
    pub addr: SocketAddr,
    good: bool,
}

impl CandidateAddress {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr, good: true }
    }

    pub fn is_good(&self) -> bool {
        self.good
    }

    pub fn mark_bad(&mut self) {
        self.good = false;
    }
}

/// A single cached DNS entry containing resolved addresses and metadata.
///
/// Each entry stores the resolved socket addresses for a hostname,
/// along with when it was resolved and its time-to-live duration.
#[derive(Debug, Clone)]
pub struct DnsEntry {
    pub endpoint: EndpointKey,
    pub addresses: Vec<CandidateAddress>,
    /// Timestamp when this entry was created/resolved
    pub resolved_at: Instant,
    /// Time-to-live for this entry before it's considered stale
    pub ttl: Duration,
    /// Whether IPv4 addresses should be preferred in ordering
    pub ipv4_preferred: bool,
}

impl DnsEntry {
    pub fn new(
        endpoint: EndpointKey,
        addresses: Vec<SocketAddr>,
        resolved_at: Instant,
        ttl: Duration,
        ipv4_preferred: bool,
    ) -> Self {
        Self {
            endpoint,
            addresses: addresses.into_iter().map(CandidateAddress::new).collect(),
            resolved_at,
            ttl,
            ipv4_preferred,
        }
    }

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
        if self.ipv4_preferred {
            self.addresses
                .iter()
                .find(|candidate| candidate.good && matches!(candidate.addr.ip(), IpAddr::V4(_)))
                .map(|candidate| candidate.addr)
                .or_else(|| {
                    self.addresses
                        .iter()
                        .find(|candidate| candidate.good)
                        .map(|candidate| candidate.addr)
                })
        } else {
            self.addresses
                .iter()
                .find(|candidate| candidate.good)
                .map(|candidate| candidate.addr)
        }
    }

    /// Return a clone of all cached addresses for this entry.
    pub fn all_addresses(&self) -> Vec<SocketAddr> {
        self.addresses
            .iter()
            .filter(|candidate| candidate.good)
            .map(|candidate| candidate.addr)
            .collect()
    }

    pub fn mark_bad(&mut self, address: SocketAddr) -> bool {
        let Some(candidate) = self
            .addresses
            .iter_mut()
            .find(|candidate| candidate.addr == address)
        else {
            return false;
        };
        candidate.mark_bad();
        true
    }
}
