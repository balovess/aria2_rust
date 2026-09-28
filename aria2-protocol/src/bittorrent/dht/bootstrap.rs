//! DHT bootstrap — resolves entry point hostnames and seeds the routing table.
//!
//! The C++ implementation uses `DHTEntryPointNameResolveCommand` with c-ares
//! for async DNS resolution of bootstrap node hostnames like
//! `router.bittorrent.com`. This Rust version uses tokio's built-in DNS
//! resolver (which delegates to the OS resolver) via `tokio::net::lookup_host`.

use super::node::DhtNode;
use super::routing_table::RoutingTable;
use std::net::SocketAddr;

/// Well-known DHT bootstrap nodes (hostname + port).
///
/// These hostnames MUST be resolved via DNS before use — they are NOT
/// IP addresses and cannot be parsed as `SocketAddr` directly.
const BOOTSTRAP_NODES: &[(&str, u16)] = &[
    ("router.bittorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.aelitis.com", 6881),
];

pub struct DhtBootstrap;

impl DhtBootstrap {
    /// Create provisional routing-table nodes for configured bootstrap endpoints.
    ///
    /// Their IDs are placeholders until the remote node responds, matching the
    /// original client's random-ID bootstrap node construction.
    pub(crate) fn nodes_from_addresses(
        addresses: impl IntoIterator<Item = SocketAddr>,
    ) -> Vec<DhtNode> {
        use rand::SeedableRng;

        let mut rng = rand::rngs::StdRng::from_entropy();
        addresses
            .into_iter()
            .map(|addr| DhtNode::unverified(random_node_id(&mut rng), addr))
            .collect()
    }

    /// Resolve all bootstrap node hostnames to socket addresses asynchronously.
    ///
    /// For each hostname, performs DNS resolution via `tokio::net::lookup_host`.
    /// Takes the first resolved address for each hostname. If resolution fails,
    /// logs a warning and skips that node (rather than falling back to `0.0.0.0:0`
    /// which would be useless).
    ///
    /// This is the equivalent of C++ `DHTEntryPointNameResolveCommand` which
    /// uses c-ares for async DNS resolution of entry point hostnames.
    pub async fn resolve_bootstrap_nodes() -> Vec<DhtNode> {
        Self::resolve_bootstrap_nodes_with_family(None).await
    }

    /// Resolve bootstrap nodes for the address family used by the UDP socket.
    pub(crate) async fn resolve_bootstrap_nodes_for_family(use_ipv6: bool) -> Vec<DhtNode> {
        Self::resolve_bootstrap_nodes_with_family(Some(use_ipv6)).await
    }

    async fn resolve_bootstrap_nodes_with_family(family: Option<bool>) -> Vec<DhtNode> {
        use rand::SeedableRng;
        // Use StdRng instead of ThreadRng to satisfy Send requirement
        // across async boundaries (ThreadRng is !Send due to Rc internals).
        let mut rng = rand::rngs::StdRng::from_entropy();
        let mut nodes = Vec::new();

        for (host, port) in BOOTSTRAP_NODES {
            // Generate a random placeholder ID for the bootstrap node.
            let id = random_node_id(&mut rng);

            // Resolve hostname via async DNS (C++ uses c-ares here).
            match tokio::net::lookup_host(format!("{}:{}", host, port)).await {
                Ok(addrs) => {
                    if let Some(addr) = select_bootstrap_address(addrs, family) {
                        tracing::debug!(
                            "Resolved DHT bootstrap node {}:{} -> {}",
                            host,
                            port,
                            addr
                        );
                        nodes.push(DhtNode::unverified(id, addr));
                    } else {
                        tracing::warn!(
                            "DNS resolution returned no matching addresses for {}:{}",
                            host,
                            port
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to resolve DHT bootstrap node {}:{}: {}",
                        host,
                        port,
                        e
                    );
                }
            }
        }

        tracing::info!(
            resolved = nodes.len(),
            total = BOOTSTRAP_NODES.len(),
            "DHT bootstrap node DNS resolution complete"
        );

        nodes
    }

    /// Resolve bootstrap nodes and add them to the routing table.
    ///
    /// Returns the number of nodes successfully added.
    pub async fn add_bootstrap_nodes_to_table(routing_table: &mut RoutingTable) -> usize {
        let nodes = Self::resolve_bootstrap_nodes().await;
        let count_before = routing_table.total_node_count();

        for node in nodes {
            routing_table.insert(node);
        }

        routing_table.total_node_count() - count_before
    }

    /// Return the list of bootstrap node hostnames as "host:port" strings.
    pub fn bootstrap_node_list() -> Vec<String> {
        BOOTSTRAP_NODES
            .iter()
            .map(|(host, port)| format!("{}:{}", host, port))
            .collect()
    }
}

fn random_node_id(rng: &mut impl rand::Rng) -> [u8; 20] {
    let mut id = [0u8; 20];
    for byte in &mut id {
        *byte = rng.r#gen();
    }
    id
}

fn select_bootstrap_address(
    addrs: impl IntoIterator<Item = std::net::SocketAddr>,
    family: Option<bool>,
) -> Option<std::net::SocketAddr> {
    addrs
        .into_iter()
        .find(|address| family.is_none_or(|use_ipv6| address.is_ipv6() == use_ipv6))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_nodes_defined() {
        assert!(!BOOTSTRAP_NODES.is_empty());
        for (host, port) in BOOTSTRAP_NODES {
            assert!(!host.is_empty());
            assert!(*port > 0);
        }
    }

    #[test]
    fn bootstrap_resolution_selects_the_bound_ip_family() {
        let ipv6 = "[2001:db8::1]:6881"
            .parse::<std::net::SocketAddr>()
            .unwrap();
        let ipv4 = "192.0.2.1:6881".parse::<std::net::SocketAddr>().unwrap();
        let resolver_order = [ipv6, ipv4];

        assert_eq!(
            super::select_bootstrap_address(resolver_order, Some(false)),
            Some(ipv4)
        );
        assert_eq!(
            super::select_bootstrap_address(resolver_order, Some(true)),
            Some(ipv6)
        );
        assert_eq!(
            super::select_bootstrap_address(resolver_order, None),
            Some(ipv6)
        );
    }

    #[tokio::test]
    async fn test_resolve_bootstrap_nodes() {
        // This test may fail in offline environments, so we just verify
        // the function completes without panicking.
        let nodes = DhtBootstrap::resolve_bootstrap_nodes().await;
        // In a networked environment, at least some should resolve.
        // In an offline environment, all may fail gracefully.
        for node in &nodes {
            assert_eq!(node.id.len(), 20);
            // Resolved nodes should NOT be 0.0.0.0:0
            assert_ne!(
                node.addr,
                "0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap()
            );
        }
    }

    #[test]
    fn test_bootstrap_list_strings() {
        let list = DhtBootstrap::bootstrap_node_list();
        assert!(!list.is_empty());
        for entry in &list {
            assert!(entry.contains(':'));
        }
    }
}
