//! DNS cache with TTL support, negative caching, and resolution logic.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};

use super::entry::DnsEntry;

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
    pub(crate) cache: HashMap<String, DnsEntry>,
    /// Default TTL for successfully resolved entries
    default_ttl: Duration,
    /// TTL for failed/negative lookups (prevents retry storms)
    negative_ttl: Duration,
    /// Negative cache: hostname -> timestamp of last failed lookup
    pub(crate) negative_entries: HashMap<String, Instant>,
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
            negative_ttl: Duration::from_secs(60), // 1 minute for failures
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
    pub async fn resolve(&mut self, hostname: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
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
        self.negative_entries
            .insert(hostname.to_string(), Instant::now());
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
    Some(TokioAsyncResolver::tokio(ResolverConfig::default(), opts))
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new()
    }
}
