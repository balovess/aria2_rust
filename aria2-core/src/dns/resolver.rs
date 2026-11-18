//! Shared DNS policy used by protocol clients and hostname-based sockets.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::dns_cache::DnsCache;
use crate::network::OutboundNetworkPolicy;

/// Resolves hostnames either through aria2's shared cache or the system resolver.
///
/// The cache is shared by clones so HTTP redirects, trackers and peer discovery
/// observe the same DNS state for a download.
#[derive(Clone)]
pub struct DnsResolver {
    cache: Arc<Mutex<DnsCache>>,
    use_cache: bool,
}

impl DnsResolver {
    pub fn system() -> Self {
        Self::new(Arc::new(Mutex::new(DnsCache::new())), false)
    }

    pub fn from_cache(cache: DnsCache, use_cache: bool) -> Self {
        Self::new(Arc::new(Mutex::new(cache)), use_cache)
    }

    pub fn new(cache: Arc<Mutex<DnsCache>>, use_cache: bool) -> Self {
        Self { cache, use_cache }
    }

    pub async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        if let Ok(ip) = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host)
            .parse()
        {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }

        if self.use_cache {
            let cache = self.cache.lock().await.clone();
            return cache
                .resolve_with_refresh(host, port)
                .await
                .map_err(|error| io::Error::other(error.to_string()));
        }

        tokio::net::lookup_host((host, port))
            .await
            .map(Iterator::collect)
    }

    /// Build a reqwest resolver that resolves every hostname, including redirect
    /// targets, through this policy and filters addresses against the route policy.
    pub fn reqwest_resolver(
        &self,
        network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Arc<dyn reqwest::dns::Resolve> {
        Arc::new(ReqwestDnsResolver {
            resolver: self.clone(),
            network_policy,
        })
    }
}

struct ReqwestDnsResolver {
    resolver: DnsResolver,
    network_policy: Arc<OutboundNetworkPolicy>,
}

impl reqwest::dns::Resolve for ReqwestDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let resolver = self.resolver.clone();
        let network_policy = Arc::clone(&self.network_policy);
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = resolver.resolve(&host, 0).await?;
            let addresses: Vec<_> = addresses
                .into_iter()
                .filter(|address| {
                    network_policy.is_direct() || network_policy.source_for(*address).is_ok()
                })
                .collect();
            if addresses.is_empty() {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("DNS returned no addresses compatible with outbound policy for {host}"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolver_returns_all_cached_dual_stack_candidates() {
        let cache = Arc::new(Mutex::new(DnsCache::new()));
        let resolver = DnsResolver::new(cache, true);
        let addresses = resolver
            .resolve("localhost", 8080)
            .await
            .expect("localhost resolves through the shared cache");

        assert!(addresses.iter().any(SocketAddr::is_ipv4));
        assert!(addresses.iter().any(SocketAddr::is_ipv6));
        assert!(addresses.iter().all(|address| address.port() == 8080));
    }

    #[tokio::test]
    async fn resolver_uses_system_mode_when_async_dns_is_disabled() {
        let resolver = DnsResolver::system();
        let addresses = resolver
            .resolve("localhost", 80)
            .await
            .expect("system resolver resolves localhost");

        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|address| address.port() == 80));
    }
}
