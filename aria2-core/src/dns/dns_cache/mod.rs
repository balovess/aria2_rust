//! DNS Cache Module
//!
//! Provides DNS resolution caching with TTL support, negative caching for failed
//! lookups (to prevent retry storms), and IPv4/IPv6 preference sorting.
//!
//! # Features
//!
//! - **TTL-based expiration**: Cached entries expire after a configurable time-to-live
//! - **Negative caching**: Failed lookups are remembered to avoid immediate retries
//! - **IPv4 preference**: Addresses can be sorted with IPv4 first (matching C++ aria2 behavior)
//! - **Dependency injection**: Cache instances are created during engine initialization
//!   and passed down, avoiding global mutable state
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2_core::dns::dns_cache::DnsCache;
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut cache = DnsCache::with_ttl(300, 60);
//!     match cache.resolve("example.com", 80).await {
//!         Ok(addrs) => println!("Resolved: {:?}", addrs),
//!         Err(e) => eprintln!("DNS error: {}", e),
//!     }
//! }
//! ```

pub mod cache;
pub mod entry;

#[cfg(test)]
mod tests;

// Re-export all public items so that `dns_cache::X` still works for external consumers.
pub use cache::DnsCache;
pub use entry::DnsEntry;
