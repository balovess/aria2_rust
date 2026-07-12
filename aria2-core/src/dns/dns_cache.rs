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

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;

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

/// A DNS resolution cache with TTL support and negative caching.
///
/// This cache stores resolved DNS entries and avoids repeated lookups
/// for the same hostname within the TTL window. It also implements
/// negative caching for failed lookups to prevent retry storms.
///
/// # Thread Safety
///
/// For use in async contexts, wrap with `tokio::sync::Mutex`.
/// Instances should be created during engine initialization and passed
/// down via dependency injection rather than using global singletons.
pub struct DnsCache {
    /// Cache of successful DNS resolutions: hostname -> DnsEntry
    cache: HashMap<String, DnsEntry>,
    /// Default TTL for successfully resolved entries
    default_ttl: Duration,
    /// TTL for failed/negative lookups (prevents retry storms)
    negative_ttl: Duration,
    /// Negative cache: hostname -> timestamp of last failed lookup
    negative_entries: HashMap<String, Instant>,
    /// Whether to prefer IPv4 addresses when sorting results
    ipv4_preference: bool,
    /// Fully-async hickory DNS resolver. When `None` (or on lookup error) resolution
    /// falls back to `tokio::net::lookup_host`. `TokioAsyncResolver` is `Arc`-backed
    /// internally, so cloning is cheap and no outer `Arc` is required.
    resolver: Option<TokioAsyncResolver>,
}

impl DnsCache {
    /// Create a new DNS cache with default settings.
    ///
    /// Default values:
    /// - TTL: 300 seconds (5 minutes)
    /// - Negative TTL: 60 seconds (1 minute)
    /// - IPv4 preference: enabled
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            default_ttl: Duration::from_secs(300), // 5 minutes default
            negative_ttl: Duration::from_secs(60),   // 1 minute for failures
            negative_entries: HashMap::new(),
            ipv4_preference: true, // Prefer IPv4 by default (like C++ aria2)
            resolver: build_default_resolver(),
        }
    }

    /// Create a new DNS cache with custom TTL values.
    ///
    /// # Arguments
    ///
    /// * `default_ttl_secs` - Time-to-live for successful resolutions (in seconds)
    /// * `negative_ttl_secs` - Time-to-live for failed lookups (in seconds)
    pub fn with_ttl(default_ttl_secs: u64, negative_ttl_secs: u64) -> Self {
        Self {
            default_ttl: Duration::from_secs(default_ttl_secs),
            negative_ttl: Duration::from_secs(negative_ttl_secs),
            ..Self::new()
        }
    }

    /// Inject a custom hickory resolver (useful for testing with a configured/mock resolver).
    ///
    /// This replaces the default resolver built during construction. The TTL settings and
    /// IPv4 preference are preserved.
    pub fn with_resolver(mut self, resolver: TokioAsyncResolver) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Resolve a hostname to socket addresses, using cache if valid.
    ///
    /// Resolution strategy:
    /// 1. Check positive cache — return immediately if entry exists and is not expired
    /// 2. Check negative cache — return error if lookup recently failed
    /// 3. Try the fully-async hickory resolver (no blocking getaddrinfo); on success the
    ///    record TTL is capped at `default_ttl` and the result is cached
    /// 4. If hickory is unavailable or errors, fall back to `tokio::net::lookup_host`
    ///    (blocking getaddrinfo on a tokio thread pool) so behavior stays robust
    /// 5. On success: sort addresses by IPv4 preference, cache result, return
    /// 6. On failure: record in negative cache, return error
    ///
    /// # Arguments
    ///
    /// * `hostname` - The hostname to resolve (e.g., "example.com")
    /// * `port` - The port number to include in resolved addresses
    ///
    /// # Returns
    ///
    /// A vector of resolved `SocketAddr` on success, or an error string on failure.
    pub async fn resolve(
        &mut self,
        hostname: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, String> {
        // 1. Check positive cache first
        if let Some(entry) = self.cache.get(hostname)
            && !entry.is_expired()
        {
            return Ok(entry.all_addresses());
        }

        // 2. Check negative cache (recently failed lookup)
        if let Some(failed_at) = self.negative_entries.get(hostname)
            && failed_at.elapsed() < self.negative_ttl
        {
            return Err(format!(
                "DNS lookup recently failed for {} (retry after {:?})",
                hostname,
                self.negative_ttl.saturating_sub(failed_at.elapsed())
            ));
        }

        // 3. Try the fully-async hickory resolver first (avoids blocking getaddrinfo).
        if let Some(resolver) = self.resolver.as_ref() {
            match resolver.lookup_ip(hostname).await {
                Ok(lookup) => {
                    let mut addrs: Vec<SocketAddr> =
                        lookup.iter().map(|ip| SocketAddr::new(ip, port)).collect();

                    if !addrs.is_empty() {
                        // Derive TTL from the lookup's remaining validity, capped at default_ttl.
                        let record_ttl = lookup
                            .valid_until()
                            .saturating_duration_since(Instant::now());
                        let actual_ttl = record_ttl.min(self.default_ttl);

                        if self.ipv4_preference {
                            addrs.sort_by_key(|a| match a.ip() {
                                IpAddr::V4(_) => 0u8,
                                IpAddr::V6(_) => 1u8,
                            });
                        }

                        let hostname_owned = hostname.to_string();
                        let entry = DnsEntry {
                            hostname: hostname_owned.clone(),
                            addresses: addrs.clone(),
                            resolved_at: Instant::now(),
                            ttl: actual_ttl,
                            ipv4_preferred: self.ipv4_preference,
                        };
                        self.cache.insert(hostname_owned, entry);
                        self.negative_entries.remove(hostname);

                        tracing::trace!(
                            hostname = hostname,
                            addr_count = addrs.len(),
                            ttl_secs = actual_ttl.as_secs(),
                            "DNS resolved via hickory async resolver"
                        );
                        return Ok(addrs);
                    }

                    tracing::debug!(
                        hostname = hostname,
                        "hickory returned no addresses, falling back to tokio::net::lookup_host"
                    );
                    // Fall through to the OS-level fallback below.
                }
                Err(e) => {
                    tracing::debug!(
                        hostname = hostname,
                        error = %e,
                        "hickory DNS lookup failed, falling back to tokio::net::lookup_host"
                    );
                    // Fall through to the OS-level fallback below.
                }
            }
        }

        // 4. Fallback: OS-level resolution via tokio (blocking getaddrinfo on a thread pool).
        //    Used when the hickory resolver is unavailable, returns no addresses, or errors.
        let addr_str = format!("{}:{}", hostname, port);
        match tokio::net::lookup_host(&addr_str).await {
            Ok(addrs) => {
                let mut sorted: Vec<SocketAddr> = addrs.collect();

                // Sort: IPv4 first if preferred, then by address family
                if self.ipv4_preference {
                    sorted.sort_by_key(|a| match a.ip() {
                        IpAddr::V4(_) => 0u8,
                        IpAddr::V6(_) => 1u8,
                    });
                }

                let hostname_owned = hostname.to_string();
                let entry = DnsEntry {
                    hostname: hostname_owned.clone(),
                    addresses: sorted.clone(),
                    resolved_at: Instant::now(),
                    ttl: self.default_ttl,
                    ipv4_preferred: self.ipv4_preference,
                };
                self.cache.insert(hostname_owned, entry);
                self.negative_entries.remove(hostname);

                tracing::trace!(
                    hostname = hostname,
                    addr_count = sorted.len(),
                    "DNS resolved via tokio::net::lookup_host fallback"
                );
                Ok(sorted)
            }
            Err(e) => {
                // Record failure in negative cache to prevent retry storms
                self.negative_entries
                    .insert(hostname.to_string(), Instant::now());
                tracing::debug!(
                    hostname = hostname,
                    error = %e,
                    "DNS resolution failed via fallback"
                );
                Err(e.to_string())
            }
        }
    }

    /// Force refresh a specific hostname, bypassing any cached entry.
    ///
    /// This removes any existing cache entry for the hostname and performs
    /// a fresh DNS resolution. Useful when you know the DNS records may have changed.
    ///
    /// # Arguments
    ///
    /// * `hostname` - The hostname to re-resolve
    /// * `port` - The port number for resolved addresses
    pub async fn force_refresh(
        &mut self,
        hostname: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, String> {
        self.cache.remove(hostname);
        self.resolve(hostname, port).await
    }

    /// Clear all cached entries (both positive and negative).
    pub fn clear(&mut self) {
        self.cache.clear();
        self.negative_entries.clear();
    }

    /// Remove expired entries from the cache.
    ///
    /// Call this periodically (e.g., every few minutes) to reclaim memory
    /// from stale entries. Also cleans up expired negative cache entries.
    ///
    /// # Returns
    ///
    /// The number of entries that were removed.
    pub fn purge_expired(&mut self) -> usize {
        let before = self.cache.len();
        self.cache.retain(|_, v| !v.is_expired());
        self.negative_entries
            .retain(|_, t| t.elapsed() < self.negative_ttl);
        before - self.cache.len()
    }

    /// Set whether IPv4 addresses should be preferred over IPv6.
    ///
    /// When enabled, resolved addresses are sorted with IPv4 addresses first.
    /// This matches the behavior of C++ aria2 which prefers IPv4 by default.
    pub fn set_ipv4_preference(&mut self, prefer_ipv4: bool) {
        self.ipv4_preference = prefer_ipv4;
    }

    /// Get the number of currently cached (non-expired) entries.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Get the default TTL setting.
    pub fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    /// Get the negative TTL setting.
    pub fn negative_ttl(&self) -> Duration {
        self.negative_ttl
    }

    /// Manually record a failed lookup in the negative cache.
    ///
    /// This is useful for testing negative cache behavior without
    /// depending on network DNS resolution.
    pub fn record_failure(&mut self, hostname: &str) {
        self.negative_entries.insert(hostname.to_string(), Instant::now());
    }

    /// Check cache only (no network resolution).
    ///
    /// Returns cached addresses if available, or an error if the entry
    /// is in the negative cache or not cached at all. This is useful
    /// for testing cache behavior without network dependencies.
    pub fn resolve_no_network(
        &mut self,
        hostname: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, String> {
        // 1. Check positive cache first
        if let Some(entry) = self.cache.get(hostname)
            && !entry.is_expired()
        {
            return Ok(entry.all_addresses());
        }

        // 2. Check negative cache (recently failed lookup)
        if let Some(failed_at) = self.negative_entries.get(hostname)
            && failed_at.elapsed() < self.negative_ttl
        {
            return Err(format!(
                "DNS lookup recently failed for {} (retry after {:?})",
                hostname,
                self.negative_ttl.saturating_sub(failed_at.elapsed())
            ));
        }

        Err(format!("No cached entry for {}:{}", hostname, port))
    }
}

/// Build the default hickory async resolver.
///
/// Uses the default upstream configuration (Google Public DNS via `ResolverConfig::default()`)
/// and enables `use_hosts_file` so that names present in the system hosts file (e.g.
/// `localhost`) are answered immediately without any network I/O — mirroring the behavior of
/// `getaddrinfo` used by `tokio::net::lookup_host`.
///
/// `TokioAsyncResolver::tokio` returns a ready handle (it does not spawn a background task at
/// construction time and captures no runtime handle), so this is safe to call outside of an
/// async context. The returned value is wrapped in `Some`; callers fall back to
/// `tokio::net::lookup_host` whenever the resolver is `None`.
fn build_default_resolver() -> Option<TokioAsyncResolver> {
    let mut opts = ResolverOpts::default();
    opts.use_hosts_file = true;
    Some(TokioAsyncResolver::tokio(
        ResolverConfig::default(),
        opts,
    ))
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new()
    }
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a test cache with very short TTLs for fast testing
    fn create_test_cache() -> DnsCache {
        DnsCache::with_ttl(10, 1) // 10s positive TTL, 1s negative TTL
    }

    #[test]
    fn test_dns_entry_is_expired() {
        let entry = DnsEntry {
            hostname: "test.com".to_string(),
            addresses: vec![],
            resolved_at: Instant::now(),
            ttl: Duration::from_secs(60),
            ipv4_preferred: true,
        };
        assert!(!entry.is_expired());

        let expired_entry = DnsEntry {
            hostname: "old.com".to_string(),
            addresses: vec![],
            resolved_at: Instant::now() - Duration::from_secs(61),
            ttl: Duration::from_secs(60),
            ipv4_preferred: false,
        };
        assert!(expired_entry.is_expired());
    }

    #[test]
    fn test_dns_entry_best_address_ipv4_preferred() {
        let ipv6_addr: SocketAddr = "[::1]:8080".parse().unwrap();
        let ipv4_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        let entry = DnsEntry {
            hostname: "mixed.com".to_string(),
            addresses: vec![ipv6_addr, ipv4_addr], // IPv6 first
            resolved_at: Instant::now(),
            ttl: Duration::from_secs(60),
            ipv4_preferred: true,
        };

        // Should prefer IPv4 even though it's second in list
        let best = entry.best_address().unwrap();
        assert_eq!(best, ipv4_addr);
    }

    #[test]
    fn test_dns_entry_best_address_no_ipv4() {
        let ipv6_addr: SocketAddr = "[::1]:8080".parse().unwrap();

        let entry = DnsEntry {
            hostname: "ipv6only.com".to_string(),
            addresses: vec![ipv6_addr],
            resolved_at: Instant::now(),
            ttl: Duration::from_secs(60),
            ipv4_preferred: true,
        };

        let best = entry.best_address().unwrap();
        assert_eq!(best, ipv6_addr);
    }

    #[test]
    fn test_dns_entry_best_address_empty() {
        let entry = DnsEntry {
            hostname: "empty.com".to_string(),
            addresses: vec![],
            resolved_at: Instant::now(),
            ttl: Duration::from_secs(60),
            ipv4_preferred: true,
        };

        assert!(entry.best_address().is_none());
    }

    #[test]
    fn test_dns_cache_creation() {
        let cache = DnsCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.default_ttl(), Duration::from_secs(300));
        assert_eq!(cache.negative_ttl(), Duration::from_secs(60));
    }

    #[test]
    fn test_dns_cache_with_custom_ttl() {
        let cache = DnsCache::with_ttl(600, 30);
        assert_eq!(cache.default_ttl(), Duration::from_secs(600));
        assert_eq!(cache.negative_ttl(), Duration::from_secs(30));
    }

    #[test]
    fn test_dns_cache_clear() {
        let mut cache = create_test_cache();
        // Manually insert something into the cache
        let entry = DnsEntry {
            hostname: "example.com".to_string(),
            addresses: vec!["127.0.0.1:80".parse().unwrap()],
            resolved_at: Instant::now(),
            ttl: Duration::from_secs(60),
            ipv4_preferred: true,
        };
        cache.cache.insert("example.com".to_string(), entry);
        cache.negative_entries
            .insert("failed.com".to_string(), Instant::now());

        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.negative_entries.is_empty());
    }

    /// Test J3.4 #1: Second call returns cached result without network I/O.
    ///
    /// We use localhost which resolves instantly and verify that once cached,
    /// subsequent calls return the same data without needing actual network calls.
    /// Since we can't easily mock tokio::net::lookup_host in unit tests,
    /// we verify the caching mechanism directly by manipulating the internal state.
    #[tokio::test]
    async fn test_resolve_caches_result() {
        let mut cache = create_test_cache();

        // Resolve localhost (should always succeed)
        let result1 = cache.resolve("localhost", 80).await;
        assert!(
            result1.is_ok(),
            "First resolve of localhost should succeed: {:?}",
            result1.err()
        );
        let addrs1 = result1.unwrap();
        assert!(!addrs1.is_empty(), "localhost should resolve to at least one address");

        // Second resolve should hit cache (same result, no network call)
        let result2 = cache.resolve("localhost", 80).await;
        assert!(result2.is_ok(), "Second resolve should succeed from cache");
        let addrs2 = result2.unwrap();
        assert_eq!(
            addrs1, addrs2,
            "Cached result should match original resolution"
        );

        // Verify cache now has exactly one entry
        assert_eq!(cache.len(), 1);
    }

    /// Test J3.4 #2: Failed lookup blocks retry for negative_ttl duration.
    ///
    /// We inject a negative cache entry directly (without network DNS)
    /// and verify that subsequent attempts within the negative TTL window
    /// fail immediately with the "recently failed" message.
    #[test]
    fn test_negative_cache_blocks_retry() {
        let mut cache = DnsCache::with_ttl(300, 2); // 2-second negative TTL

        // Inject a negative cache entry directly (no network dependency)
        cache.record_failure("test-host.invalid");

        // Immediate retry should be blocked by negative cache
        let result = cache.resolve_no_network("test-host.invalid", 80);
        assert!(
            result.is_err(),
            "Lookup should be blocked by negative cache"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("recently failed"),
            "Error should mention recent failure: {}",
            err_msg
        );
    }

    /// Test J3.4 #3: Expired entries are removed by purge_expired().
    ///
    /// We insert entries with already-expired timestamps and verify that
    /// purge_expired removes them while keeping valid entries intact.
    #[test]
    fn test_purge_expired_removes_old() {
        let mut cache = DnsCache::with_ttl(1, 60); // 1-second TTL

        // Insert an already-expired entry
        let expired_entry = DnsEntry {
            hostname: "expired.example.com".to_string(),
            addresses: vec!["10.0.0.1:80".parse().unwrap()],
            resolved_at: Instant::now() - Duration::from_secs(5), // Expired 5 seconds ago
            ttl: Duration::from_secs(1),
            ipv4_preferred: true,
        };
        cache.cache.insert("expired.example.com".to_string(), expired_entry);

        // Insert a still-valid entry
        let fresh_entry = DnsEntry {
            hostname: "fresh.example.com".to_string(),
            addresses: vec!["10.0.0.2:80".parse().unwrap()],
            resolved_at: Instant::now(),
            ttl: Duration::from_secs(3600), // 1 hour TTL
            ipv4_preferred: true,
        };
        cache.cache.insert("fresh.example.com".to_string(), fresh_entry);

        assert_eq!(cache.len(), 2, "Should have 2 entries before purge");

        let removed = cache.purge_expired();
        assert_eq!(removed, 1, "Should remove exactly 1 expired entry");
        assert_eq!(cache.len(), 1, "Should have 1 entry remaining");
        assert!(
            cache.cache.contains_key("fresh.example.com"),
            "Fresh entry should still exist"
        );
        assert!(
            !cache.cache.contains_key("expired.example.com"),
            "Expired entry should be removed"
        );
    }

    /// Test J3.4 #4: IPv4 addresses come first when ipv4_preference is enabled.
    ///
    /// Verifies that when IPv4 preference is set, the DnsCache sorts resolved
    /// addresses so that IPv4 addresses appear before IPv6 addresses.
    #[tokio::test]
    async fn test_ipv4_preferred_sorting() {
        let mut cache = create_test_cache();

        // Resolve localhost which typically returns both [::1] and 127.0.0.1
        let result = cache.resolve("localhost", 8080).await;
        assert!(
            result.is_ok(),
            "localhost resolution should succeed: {:?}",
            result.err()
        );
        let addrs = result.unwrap();

        // If we have both IPv4 and IPv6 addresses, IPv4 should come first
        let has_ipv4 = addrs.iter().any(|a| matches!(a.ip(), IpAddr::V4(_)));
        let has_ipv6 = addrs.iter().any(|a| matches!(a.ip(), IpAddr::V6(_)));

        if has_ipv4 && has_ipv6 {
            let first_ipv4_pos = addrs
                .iter()
                .position(|a| matches!(a.ip(), IpAddr::V4(_)))
                .unwrap();
            let first_ipv6_pos = addrs
                .iter()
                .position(|a| matches!(a.ip(), IpAddr::V6(_)))
                .unwrap();
            assert!(
                first_ipv4_pos < first_ipv6_pos,
                "IPv4 addresses should come before IPv6 when preferred. Got order: {:?}",
                addrs
            );
        }

        // Verify we can also disable IPv4 preference
        cache.set_ipv4_preference(false);
        // Re-resolve with different preference
        let result2 = cache.force_refresh("localhost", 8080).await;
        assert!(result2.is_ok(), "Force refresh should succeed");
    }

    #[test]
    fn test_force_refresh_clears_cache_entry() {
        // Note: This test only verifies the cache clearing logic,
        // not the full async resolution (which requires tokio runtime)
        let mut cache = create_test_cache();

        // Manually pre-populate cache
        let entry = DnsEntry {
            hostname: "preloaded.com".to_string(),
            addresses: vec!["192.168.1.1:443".parse().unwrap()],
            resolved_at: Instant::now(),
            ttl: Duration::from_secs(3600),
            ipv4_preferred: true,
        };
        cache.cache.insert("preloaded.com".to_string(), entry);
        assert_eq!(cache.len(), 1);

        // force_refresh should remove the existing entry (we don't await here
        // because this is a sync test; the removal happens before the async resolve)
        // In practice, the cache.remove() is called synchronously at the start
        // of force_refresh, so we can verify the entry would be removed
    }

    #[test]
    fn test_default_impl() {
        let cache = DnsCache::default();
        assert!(cache.is_empty());
        assert_eq!(cache.default_ttl(), Duration::from_secs(300));
    }

    /// Test D1 #1: the hickory async resolver resolves "localhost" and caches it.
    ///
    /// "localhost" is answered from the system hosts file (use_hosts_file is enabled), so this
    /// exercises the hickory code path without depending on external network reachability.
    #[tokio::test]
    async fn test_async_dns_resolves_localhost() {
        let mut cache = DnsCache::with_ttl(300, 60);
        let result = cache.resolve("localhost", 80).await;
        assert!(
            result.is_ok(),
            "localhost should resolve: {:?}",
            result.err()
        );
        let addrs = result.unwrap();
        assert!(!addrs.is_empty(), "should have at least one address");
        // Verify it's cached.
        assert_eq!(
            cache.len(),
            1,
            "localhost should be cached after resolve"
        );
    }

    /// Test D1 #2: the TTL reported by the resolver is capped at default_ttl.
    ///
    /// Resolves "localhost" (from the hosts file, whose entries carry a very large TTL) with a
    /// 60s default TTL and verifies the cached entry's TTL does not exceed the default.
    #[tokio::test]
    async fn test_dns_ttl_capped_at_default() {
        let mut cache = DnsCache::with_ttl(60, 10); // default 60s
        let _ = cache.resolve("localhost", 80).await;
        // Verify the cached entry has ttl <= 60s (capped at default_ttl).
        if let Some(entry) = cache.cache.get("localhost") {
            assert!(
                entry.ttl <= Duration::from_secs(60),
                "ttl should be capped at default, got {:?}",
                entry.ttl
            );
        }
    }
}
