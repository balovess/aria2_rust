//! DNS cache with TTL support, negative caching, and resolution logic.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::time::{Duration, Instant};

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{
    NameServerConfig, ResolveHosts, ResolverConfig, ResolverOpts, ServerOrderingStrategy,
};
use hickory_resolver::net::runtime::TokioRuntimeProvider;

use super::entry::DnsEntry;
use crate::error::Aria2Error;
use crate::network::{ConnectionContext, EndpointKey};

/// A DNS resolution cache with TTL support and negative caching.
///
/// This cache stores resolved DNS entries and avoids repeated lookups
/// for the same hostname within the TTL window. It also implements
/// negative caching for failed lookups to prevent retry storms.
///
/// # Thread Safety
///
/// Clones share cache entries and per-endpoint lookup coordination. The
/// resolver performs network I/O without holding the cache-state lock.
/// Instances should be created during engine initialization and passed down
/// via dependency injection rather than using global singletons.
#[derive(Clone)]
pub struct DnsCache {
    pub(super) state: Arc<RwLock<DnsCacheState>>,
    /// Default TTL for successfully resolved entries
    default_ttl: Duration,
    /// TTL for failed/negative lookups (prevents retry storms)
    negative_ttl: Duration,
    /// Whether to prefer IPv4 addresses when sorting results
    ipv4_preference: Arc<AtomicBool>,
    /// Maximum time allowed for the complete DNS lookup, including fallback.
    dns_timeout: Duration,
    /// Explicit DNS servers, or an empty list when system defaults are used.
    dns_server_addresses: Arc<[SocketAddr]>,
    /// Fully-async Hickory DNS resolver. When system DNS is in use, an unavailable
    /// resolver or lookup error falls back to `tokio::net::lookup_host`. Explicit
    /// nameserver configuration never falls back to the system resolver. `TokioResolver`
    /// is `Arc`-backed internally, so cloning is cheap and no outer `Arc` is required.
    resolver: Option<Arc<TokioResolver>>,
}

#[derive(Default)]
pub(super) struct DnsCacheState {
    pub(super) cache: HashMap<EndpointKey, DnsEntry>,
    pub(super) negative_entries: HashMap<EndpointKey, Instant>,
    lookup_flights: HashMap<EndpointKey, Weak<LookupFlight>>,
}

struct LookupFlight {
    lock: tokio::sync::Mutex<()>,
    generation: std::sync::atomic::AtomicU64,
}

impl LookupFlight {
    fn new() -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl DnsCache {
    const DEFAULT_DNS_TIMEOUT: Duration = Duration::from_secs(30);

    /// Create a new DNS cache with default settings.
    ///
    /// Default values:
    /// - TTL: 300 seconds (5 minutes)
    /// - Negative TTL: 60 seconds (1 minute)
    /// - DNS timeout: 30 seconds (matching aria2_original)
    /// - IPv4 preference: enabled
    pub fn new() -> Self {
        Self::with_ttl_and_dns_timeout(300, 60, Self::DEFAULT_DNS_TIMEOUT)
    }

    /// Create a new DNS cache with custom TTL values.
    ///
    /// # Arguments
    ///
    /// * `default_ttl_secs` - Time-to-live for successful resolutions (in seconds)
    /// * `negative_ttl_secs` - Time-to-live for failed lookups (in seconds)
    pub fn with_ttl(default_ttl_secs: u64, negative_ttl_secs: u64) -> Self {
        Self::with_ttl_and_dns_timeout(
            default_ttl_secs,
            negative_ttl_secs,
            Self::DEFAULT_DNS_TIMEOUT,
        )
    }

    /// Create a cache with an explicit DNS lookup timeout.
    pub fn with_dns_timeout(timeout: Duration) -> Self {
        Self::with_ttl_and_dns_timeout(300, 60, timeout)
    }

    /// Create a cache using explicit UDP DNS servers.
    pub fn with_dns_servers(timeout: Duration, servers: Vec<SocketAddr>) -> Self {
        let mut cache = Self::with_dns_timeout(timeout);
        cache.resolver = build_resolver(timeout, Some(&servers)).map(Arc::new);
        cache.dns_server_addresses = Arc::from(servers);
        cache
    }

    /// Parse aria2's comma-separated IP address syntax for `async-dns-server`.
    pub fn parse_dns_server_list(raw: &str) -> Result<Vec<SocketAddr>, String> {
        raw.split(',')
            .map(str::trim)
            .map(|value| {
                if value.is_empty() {
                    return Err("DNS server list contains an empty address".to_string());
                }
                value
                    .parse::<IpAddr>()
                    .map(|ip| SocketAddr::new(ip, 53))
                    .map_err(|error| format!("invalid DNS server address '{}': {}", value, error))
            })
            .collect()
    }

    fn with_ttl_and_dns_timeout(
        default_ttl_secs: u64,
        negative_ttl_secs: u64,
        dns_timeout: Duration,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(DnsCacheState::default())),
            default_ttl: Duration::from_secs(default_ttl_secs),
            negative_ttl: Duration::from_secs(negative_ttl_secs),
            ipv4_preference: Arc::new(AtomicBool::new(true)), // Like C++ aria2.
            dns_timeout,
            dns_server_addresses: Arc::from([]),
            resolver: build_resolver(dns_timeout, None).map(Arc::new),
        }
    }

    fn read_state(&self) -> RwLockReadGuard<'_, DnsCacheState> {
        self.state.read().unwrap_or_else(|error| error.into_inner())
    }

    fn write_state(&self) -> RwLockWriteGuard<'_, DnsCacheState> {
        self.state
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn lookup_flight(&self, endpoint: &EndpointKey) -> Arc<LookupFlight> {
        let mut state = self.write_state();
        state
            .lookup_flights
            .retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = state.lookup_flights.get(endpoint).and_then(Weak::upgrade) {
            return flight;
        }

        let flight = Arc::new(LookupFlight::new());
        state
            .lookup_flights
            .insert(endpoint.clone(), Arc::downgrade(&flight));
        flight
    }

    /// Inject a custom hickory resolver (useful for testing with a configured/mock resolver).
    ///
    /// This replaces the default resolver built during construction. The TTL settings and
    /// IPv4 preference are preserved.
    pub fn with_resolver(mut self, resolver: TokioResolver) -> Self {
        self.resolver = Some(Arc::new(resolver));
        self
    }

    /// Resolve a hostname to socket addresses, using cache if valid.
    ///
    /// Resolution strategy:
    /// 1. Check positive cache — return immediately if entry exists and is not expired
    /// 2. Check negative cache — return error if lookup recently failed
    /// 3. Try the fully-async Hickory resolver (no blocking getaddrinfo); on success the
    ///    record TTL is capped at `default_ttl` and the result is cached
    /// 4. If system DNS is in use and Hickory is unavailable or errors, fall back to
    ///    `tokio::net::lookup_host` (blocking getaddrinfo on a tokio thread pool)
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
    /// A vector of resolved `SocketAddr` on success, or a structured `NameResolve` error on failure.
    pub async fn resolve(&self, hostname: &str, port: u16) -> Result<Vec<SocketAddr>, Aria2Error> {
        self.purge_expired();
        let endpoint = EndpointKey::new(hostname, port);
        if let Some(result) = self.cached_result(&endpoint, hostname) {
            return result;
        }

        let flight = self.lookup_flight(&endpoint);
        let _lookup_guard = flight.lock.lock().await;
        self.purge_expired();
        if let Some(result) = self.cached_result(&endpoint, hostname) {
            return result;
        }

        let generation = flight.generation.load(Ordering::Acquire);
        let lookup = self.lookup_uncached(hostname, port).await;
        let mut state = self.write_state();
        if flight.generation.load(Ordering::Acquire) == generation {
            match &lookup {
                Ok((addresses, ttl)) => {
                    let entry = DnsEntry::new(
                        endpoint.clone(),
                        addresses.clone(),
                        Instant::now(),
                        *ttl,
                        self.ipv4_preference.load(Ordering::Relaxed),
                    );
                    state.cache.insert(endpoint.clone(), entry);
                    state.negative_entries.remove(&endpoint);
                }
                Err(_) => {
                    state.negative_entries.insert(endpoint, Instant::now());
                }
            }
        }

        lookup.map(|(addresses, _)| addresses)
    }

    fn cached_result(
        &self,
        endpoint: &EndpointKey,
        hostname: &str,
    ) -> Option<Result<Vec<SocketAddr>, Aria2Error>> {
        let state = self.read_state();
        if let Some(entry) = state.cache.get(endpoint)
            && !entry.is_expired()
        {
            return Some(Ok(entry.all_addresses()));
        }
        if let Some(failed_at) = state.negative_entries.get(endpoint)
            && failed_at.elapsed() < self.negative_ttl
        {
            return Some(Err(Aria2Error::NameResolve(format!(
                "DNS lookup recently failed for {} (retry after {:?})",
                hostname,
                self.negative_ttl.saturating_sub(failed_at.elapsed())
            ))));
        }
        None
    }

    async fn lookup_uncached(
        &self,
        hostname: &str,
        port: u16,
    ) -> Result<(Vec<SocketAddr>, Duration), Aria2Error> {
        let lookup_started = Instant::now();
        let mut hickory_failure = None;
        if let Some(resolver) = self.resolver.as_ref() {
            match tokio::time::timeout(self.dns_timeout, resolver.lookup_ip(hostname)).await {
                Err(error) => {
                    hickory_failure = Some(format!("Hickory lookup timed out: {error}"));
                    tracing::debug!(
                        hostname = hostname,
                        timeout = ?self.dns_timeout,
                        error = %error,
                        "Hickory DNS lookup timed out"
                    );
                }
                Ok(Err(error)) => {
                    hickory_failure = Some(error.to_string());
                    tracing::debug!(
                        hostname = hostname,
                        error = %error,
                        "Hickory DNS lookup failed"
                    );
                    // Fall through to the OS-level fallback below.
                }
                Ok(Ok(lookup)) => {
                    let mut addrs: Vec<SocketAddr> =
                        lookup.iter().map(|ip| SocketAddr::new(ip, port)).collect();

                    if !addrs.is_empty() {
                        // Derive TTL from the lookup's remaining validity, capped at default_ttl.
                        let record_ttl = lookup
                            .valid_until()
                            .saturating_duration_since(Instant::now());
                        let actual_ttl = record_ttl.min(self.default_ttl);

                        let ipv4_preference = self.ipv4_preference.load(Ordering::Relaxed);
                        if ipv4_preference {
                            addrs.sort_by_key(|a| match a.ip() {
                                IpAddr::V4(_) => 0u8,
                                IpAddr::V6(_) => 1u8,
                            });
                        }

                        tracing::trace!(
                            hostname = hostname,
                            addr_count = addrs.len(),
                            ttl_secs = actual_ttl.as_secs(),
                            "DNS resolved via hickory async resolver"
                        );
                        return Ok((addrs, actual_ttl));
                    }

                    hickory_failure = Some("Hickory returned no addresses".to_string());
                    tracing::debug!(hostname = hostname, "Hickory returned no addresses");
                    // Fall through to the OS-level fallback below.
                }
            }
        }

        // 4. Fallback: OS-level resolution via tokio (blocking getaddrinfo on a thread pool).
        //    Used when the hickory resolver is unavailable, returns no addresses, or errors.
        if !self.dns_server_addresses.is_empty() {
            let reason = hickory_failure
                .unwrap_or_else(|| "the configured resolver is unavailable".to_string());
            tracing::debug!(
                hostname = hostname,
                error = %reason,
                "DNS resolution failed using explicitly configured servers"
            );
            return Err(Aria2Error::NameResolve(format!(
                "DNS resolution failed for {} using explicitly configured servers: {}",
                hostname, reason
            )));
        }

        let addr_str = format!("{}:{}", hostname, port);
        let remaining_timeout = self.dns_timeout.saturating_sub(lookup_started.elapsed());
        let fallback = tokio::time::timeout(remaining_timeout, tokio::net::lookup_host(&addr_str))
            .await
            .map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("DNS lookup timed out after {:?}: {error}", self.dns_timeout),
                )
            })
            .and_then(|result| result);
        match fallback {
            Ok(addrs) => {
                let mut sorted: Vec<SocketAddr> = addrs.collect();
                let ipv4_preference = self.ipv4_preference.load(Ordering::Relaxed);

                // Sort: IPv4 first if preferred, then by address family
                if ipv4_preference {
                    sorted.sort_by_key(|a| match a.ip() {
                        IpAddr::V4(_) => 0u8,
                        IpAddr::V6(_) => 1u8,
                    });
                }

                tracing::trace!(
                    hostname = hostname,
                    addr_count = sorted.len(),
                    "DNS resolved via tokio::net::lookup_host fallback"
                );
                Ok((sorted, self.default_ttl))
            }
            Err(e) => {
                tracing::debug!(
                    hostname = hostname,
                    error = %e,
                    "DNS resolution failed via fallback"
                );
                Err(Aria2Error::NameResolve(format!(
                    "DNS resolution failed for {}: {}",
                    hostname, e
                )))
            }
        }
    }

    /// Resolve an endpoint and refresh it when every cached candidate is bad.
    ///
    /// The original connection path removes an exhausted DNS cache entry
    /// before starting the next resolution. Keeping that transition here
    /// gives protocol callers one small seam instead of duplicating the empty
    /// candidate fallback.
    pub async fn resolve_with_refresh(
        &self,
        hostname: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, Aria2Error> {
        let addresses = self.resolve(hostname, port).await?;
        if !addresses.is_empty() {
            return Ok(addresses);
        }

        self.remove_cached(hostname, port);
        self.resolve(hostname, port).await
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
        &self,
        hostname: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, Aria2Error> {
        self.remove_cached(hostname, port);
        self.resolve(hostname, port).await
    }

    /// Clear all cached entries (both positive and negative).
    pub fn clear(&self) {
        let mut state = self.write_state();
        state.cache.clear();
        state.negative_entries.clear();
        for flight in state.lookup_flights.values().filter_map(Weak::upgrade) {
            flight.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Mark one resolved address as bad without discarding other addresses.
    ///
    /// This mirrors C++ `DnsCache::markBad(hostname, ipaddr, port)`: the
    /// address is retained as a bad candidate while good addresses remain
    /// available for retry.
    pub fn mark_bad(&self, hostname: &str, address: SocketAddr) {
        self.mark_bad_context(&ConnectionContext::new(hostname, address.port(), address));
    }

    /// Mark the peer selected by a concrete connection context as bad.
    pub fn mark_bad_context(&self, context: &ConnectionContext) {
        let mut state = self.write_state();
        if let Some(entry) = state.cache.get_mut(&context.endpoint) {
            entry.mark_bad(context.peer_addr);
        }
        Self::invalidate_lookup(&mut state, &context.endpoint);
    }

    /// Return whether at least one good cached address remains.
    pub fn has_good_address(&self, endpoint: &EndpointKey) -> bool {
        self.read_state()
            .cache
            .get(endpoint)
            .is_some_and(|entry| !entry.all_addresses().is_empty())
    }

    /// Remove every cached address for a hostname and port.
    ///
    /// This mirrors C++ `DownloadEngine::removeCachedIPAddress` and also
    /// clears the negative-cache marker so the next connection performs a
    /// fresh lookup.
    pub fn remove_cached(&self, hostname: &str, port: u16) {
        let endpoint = EndpointKey::new(hostname, port);
        let mut state = self.write_state();
        state.cache.remove(&endpoint);
        state.negative_entries.remove(&endpoint);
        Self::invalidate_lookup(&mut state, &endpoint);
    }

    /// Remove expired entries from the cache.
    ///
    /// Call this periodically (e.g., every few minutes) to reclaim memory
    /// from stale entries. Also cleans up expired negative cache entries.
    ///
    /// # Returns
    ///
    /// The number of entries that were removed.
    pub fn purge_expired(&self) -> usize {
        let mut state = self.write_state();
        let before = state.cache.len();
        state.cache.retain(|_, v| !v.is_expired());
        state
            .negative_entries
            .retain(|_, t| t.elapsed() < self.negative_ttl);
        state
            .lookup_flights
            .retain(|_, flight| flight.strong_count() > 0);
        before - state.cache.len()
    }

    /// Set whether IPv4 addresses should be preferred over IPv6.
    ///
    /// When enabled, resolved addresses are sorted with IPv4 addresses first.
    /// This matches the behavior of C++ aria2 which prefers IPv4 by default.
    pub fn set_ipv4_preference(&self, prefer_ipv4: bool) {
        self.ipv4_preference.store(prefer_ipv4, Ordering::Relaxed);
    }

    /// Get the number of currently cached (non-expired) entries.
    pub fn len(&self) -> usize {
        self.read_state().cache.len()
    }

    /// Check if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.read_state().cache.is_empty()
    }

    /// Get the default TTL setting.
    pub fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    /// Get the negative TTL setting.
    pub fn negative_ttl(&self) -> Duration {
        self.negative_ttl
    }

    /// Get the maximum time allowed for one DNS lookup.
    pub fn dns_timeout(&self) -> Duration {
        self.dns_timeout
    }

    /// Get the explicit DNS servers configured for this cache.
    pub fn dns_server_addresses(&self) -> &[SocketAddr] {
        &self.dns_server_addresses
    }

    /// Manually record a failed lookup in the negative cache.
    ///
    /// This is useful for testing negative cache behavior without
    /// depending on network DNS resolution.
    pub fn record_failure(&self, hostname: &str, port: u16) {
        self.write_state()
            .negative_entries
            .insert(EndpointKey::new(hostname, port), Instant::now());
    }

    /// Check cache only (no network resolution).
    ///
    /// Returns cached addresses if available, or an error if the entry
    /// is in the negative cache or not cached at all. This is useful
    /// for testing cache behavior without network dependencies.
    pub fn resolve_no_network(
        &self,
        hostname: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, Aria2Error> {
        let endpoint = EndpointKey::new(hostname, port);
        let state = self.read_state();
        // 1. Check positive cache first
        if let Some(entry) = state.cache.get(&endpoint)
            && !entry.is_expired()
        {
            return Ok(entry.all_addresses());
        }

        // 2. Check negative cache (recently failed lookup)
        if let Some(failed_at) = state.negative_entries.get(&endpoint)
            && failed_at.elapsed() < self.negative_ttl
        {
            return Err(Aria2Error::NameResolve(format!(
                "DNS lookup recently failed for {} (retry after {:?})",
                hostname,
                self.negative_ttl.saturating_sub(failed_at.elapsed())
            )));
        }

        Err(Aria2Error::NameResolve(format!(
            "No cached entry for {}:{}",
            hostname, port
        )))
    }

    fn invalidate_lookup(state: &mut DnsCacheState, endpoint: &EndpointKey) {
        if let Some(flight) = state.lookup_flights.get(endpoint).and_then(Weak::upgrade) {
            flight.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Build the default hickory async resolver.
///
/// Uses the operating system's DNS configuration unless explicit servers are provided, and
/// enables `use_hosts_file` so that names present in the system hosts file (e.g. `localhost`)
/// are answered immediately without network I/O.
///
/// `TokioResolver` construction returns a ready handle (it does not spawn a background task at
/// construction time and captures no runtime handle), so this is safe to call outside of an
/// async context. The returned value is wrapped in `Some`; callers fall back to
/// `tokio::net::lookup_host` whenever the resolver is `None`.
fn build_resolver(dns_timeout: Duration, servers: Option<&[SocketAddr]>) -> Option<TokioResolver> {
    let (config, mut opts) = match servers {
        Some(servers) => {
            let nameservers = servers
                .iter()
                .copied()
                .map(|address| {
                    let mut config = NameServerConfig::udp(address.ip());
                    config.connections[0].port = address.port();
                    config
                })
                .collect::<Vec<_>>();
            (
                ResolverConfig::from_parts(None, Vec::new(), nameservers),
                ResolverOpts::default(),
            )
        }
        None => hickory_resolver::system_conf::read_system_conf().ok()?,
    };
    opts.use_hosts_file = ResolveHosts::Always;
    // aria2's configured DNS list is primary/backup. Hickory defaults to
    // querying two name servers concurrently, which duplicates every A/AAAA
    // query when two servers are configured.
    opts.num_concurrent_reqs = 1;
    opts.server_ordering_strategy = ServerOrderingStrategy::UserProvidedOrder;
    // Bound an individual Hickory request while leaving time for backup servers. Hickory can
    // spend the full per-request timeout before trying the next configured server, so using
    // the whole lookup deadline here leaves no room for primary/backup failover.
    opts.timeout = dns_timeout.min(Duration::from_secs(5)).min(dns_timeout / 2);
    TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
        .with_options(opts)
        .build()
        .ok()
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;

    #[test]
    fn lookup_flights_are_scoped_by_endpoint() {
        let cache = DnsCache::new();
        let endpoint = EndpointKey::new("same.test", 80);
        let same_endpoint = cache.lookup_flight(&endpoint);
        let same_endpoint_again = cache.lookup_flight(&endpoint);
        let other_endpoint = cache.lookup_flight(&EndpointKey::new("other.test", 80));

        assert!(Arc::ptr_eq(&same_endpoint, &same_endpoint_again));
        let _held = same_endpoint.lock.try_lock().unwrap();
        assert!(same_endpoint_again.lock.try_lock().is_err());
        assert!(other_endpoint.lock.try_lock().is_ok());
    }
}
