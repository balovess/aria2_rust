//! DHT bootstrap — resolves entry point hostnames and seeds the routing table.
//!
//! The C++ implementation uses `DHTEntryPointNameResolveCommand` with c-ares
//! for async DNS resolution of bootstrap node hostnames like
//! `router.bittorrent.com`. This Rust version uses tokio's built-in DNS
//! resolver (which delegates to the OS resolver) via `tokio::net::lookup_host`.

use super::node::DhtNode;
use super::routing_table::RoutingTable;

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
        use rand::{Rng, SeedableRng};
        // Use StdRng instead of ThreadRng to satisfy Send requirement
        // across async boundaries (ThreadRng is !Send due to Rc internals).
        let mut rng = rand::rngs::StdRng::from_entropy();
        let mut nodes = Vec::new();

        for (host, port) in BOOTSTRAP_NODES {
            // Generate a random node ID for the bootstrap node.
            let mut id = [0u8; 20];
            for byte in id.iter_mut() {
                *byte = rng.r#gen();
            }

            // Resolve hostname via async DNS (C++ uses c-ares here).
            match tokio::net::lookup_host(format!("{}:{}", host, port)).await {
                Ok(mut addrs) => {
                    if let Some(addr) = addrs.next() {
                        tracing::debug!(
                            "Resolved DHT bootstrap node {}:{} -> {}",
                            host,
                            port,
                            addr
                        );
                        nodes.push(DhtNode::new(id, addr));
                    } else {
                        tracing::warn!(
                            "DNS resolution returned no addresses for {}:{}",
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

    /// Synchronous fallback: returns bootstrap nodes with placeholder addresses.
    ///
    /// **WARNING**: This method does NOT perform DNS resolution. It should only
    /// be used when async resolution is not possible (e.g., in synchronous
    /// contexts). The returned nodes will have `0.0.0.0:0` addresses and will
    /// NOT be reachable. Prefer [`resolve_bootstrap_nodes`] in all async contexts.
    pub fn get_bootstrap_nodes_unreachable() -> Vec<DhtNode> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        BOOTSTRAP_NODES
            .iter()
            .map(|(host, port)| {
                let mut id = [0u8; 20];
                for byte in id.iter_mut() {
                    *byte = rng.r#gen();
                }
                tracing::warn!(
                    "Using unreachable placeholder for DHT bootstrap node {}:{} \
                     (use resolve_bootstrap_nodes() for proper DNS resolution)",
                    host,
                    port
                );
                DhtNode::new(id, "0.0.0.0:0".parse().unwrap())
            })
            .collect()
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
            assert_ne!(node.addr, "0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap());
        }
    }

    #[test]
    fn test_get_bootstrap_nodes_unreachable() {
        let nodes = DhtBootstrap::get_bootstrap_nodes_unreachable();
        assert_eq!(nodes.len(), BOOTSTRAP_NODES.len());
        for node in &nodes {
            assert_eq!(node.id.len(), 20);
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
