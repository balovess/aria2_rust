use super::node::DhtNode;
use super::routing_table::RoutingTable;

const BOOTSTRAP_NODES: &[(&str, u16)] = &[
    ("router.bittorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.aelitis.com", 6881),
];

pub struct DhtBootstrap;

impl DhtBootstrap {
    pub fn get_bootstrap_nodes() -> Vec<DhtNode> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        BOOTSTRAP_NODES.iter().map(|(host, port)| {
            let mut id = [0u8; 20];
            for byte in id.iter_mut() { *byte = rng.gen(); }
            if let Ok(addr) = format!("{}:{}", host, port).parse::<std::net::SocketAddr>() {
                DhtNode::new(id, addr)
            } else {
                DhtNode::new(id, "0.0.0.0:0".parse().unwrap())
            }
        }).collect()
    }

    pub fn add_bootstrap_nodes_to_table(routing_table: &mut RoutingTable) -> usize {
        let nodes = Self::get_bootstrap_nodes();
        let count_before = routing_table.total_node_count();

        for node in nodes {
            routing_table.insert(node);
        }

        routing_table.total_node_count() - count_before
    }

    pub fn bootstrap_node_list() -> Vec<String> {
        BOOTSTRAP_NODES.iter()
            .map(|(host, port)| format!("{}:{}", host, port))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_bootstrap_nodes() {
        let nodes = DhtBootstrap::get_bootstrap_nodes();
        assert_eq!(nodes.len(), BOOTSTRAP_NODES.len());
        for node in &nodes {
            assert_eq!(node.id.len(), 20);
        }
    }

    #[test]
    fn test_add_to_routing_table() {
        let mut table = RoutingTable::new([0u8; 20]);
        let added = DhtBootstrap::add_bootstrap_nodes_to_table(&mut table);
        assert_eq!(added, BOOTSTRAP_NODES.len());
        assert_eq!(table.total_node_count(), BOOTSTRAP_NODES.len());
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
