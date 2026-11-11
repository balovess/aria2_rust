use super::*;
use std::net::SocketAddr;

fn make_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{}", port).parse().unwrap()
}

#[test]
fn test_routing_table_creation() {
    let table = RoutingTable::new([0u8; 20]);
    assert_eq!(table.total_node_count(), 0);
    assert_eq!(table.num_buckets(), 1);
}

#[test]
fn test_insert_and_find() {
    let mut table = RoutingTable::new([0x80u8; 20]);
    let node = DhtNode::new([0xFFu8; 20], make_addr(6881));
    table.insert(node);

    assert_eq!(table.total_node_count(), 1);

    let target = [0xFFu8; 20];
    let closest = table.find_closest(&target, 5);
    assert_eq!(closest.len(), 1);
}

#[test]
fn bucket_lookup_returns_a_covering_leaf_for_any_id_range() {
    let mut table = RoutingTable::new([0u8; 20]);
    for i in 1..=super::super::bucket::K as u8 {
        table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
    }
    table.insert(DhtNode::new([0x80; 20], make_addr(6999)));

    let low_id = [0u8; 20];
    let high_id = [0xFF; 20];
    assert!(table.get_bucket_for(&low_id).is_in_range(&low_id));
    assert!(table.get_bucket_for(&high_id).is_in_range(&high_id));
}

#[test]
fn test_remove_node() {
    let mut table = RoutingTable::new([0u8; 20]);
    let id = [1u8; 20];
    table.insert(DhtNode::new(id, make_addr(6881)));
    assert_eq!(table.total_node_count(), 1);
    assert!(table.remove(&id));
    assert_eq!(table.total_node_count(), 0);
}

#[test]
fn test_bucket_split_on_insert() {
    let mut table = RoutingTable::new([0u8; 20]);

    // Fill the initial bucket with K nodes.
    for i in 1..=super::super::bucket::K as u8 {
        let node = DhtNode::new([i; 20], make_addr(6881 + i as u16));
        table.insert(node);
    }
    assert_eq!(table.total_node_count(), super::super::bucket::K);

    // Adding one more node should trigger a split if local ID is in range.
    let extra = DhtNode::new([0x80u8; 20], make_addr(9999));
    table.insert(extra);

    // The node should be added (possibly after split).
    assert!(table.total_node_count() >= super::super::bucket::K);
}

#[test]
fn bucket_split_preserves_discovered_node_verification_state() {
    let mut table = RoutingTable::new([0u8; 20]);
    for i in 1..=super::super::bucket::K as u8 {
        table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
    }

    table.insert(DhtNode::unverified([0x80u8; 20], make_addr(9999)));

    assert_eq!(table.total_node_count(), super::super::bucket::K + 1);
    assert_eq!(
        table.good_node_count(),
        super::super::bucket::K,
        "inserting across a bucket split must not promote an unverified node"
    );
}

#[test]
fn test_mark_good() {
    let mut table = RoutingTable::new([0u8; 20]);
    let id = [1u8; 20];
    let mut node = DhtNode::new(id, make_addr(6881));
    node.record_failure();
    node.record_failure();
    table.insert(node);

    assert!(table.mark_good(&id));
}

#[test]
fn test_mark_bad() {
    let mut table = RoutingTable::new([0u8; 20]);
    let id = [2u8; 20];
    table.insert(DhtNode::new(id, make_addr(6881)));

    for _ in 0..5 {
        assert!(table.mark_bad(&id));
    }
}

#[test]
fn test_get_random_node() {
    let mut table = RoutingTable::new([0u8; 20]);
    for i in 0..5u8 {
        table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
    }

    let node = table.get_random_node();
    assert!(node.is_some());
}

#[test]
fn test_get_random_node_empty_table() {
    let table = RoutingTable::new([0u8; 20]);
    let node = table.get_random_node();
    assert!(node.is_none());
}

#[test]
fn test_collect_good_nodes() {
    let mut table = RoutingTable::new([0u8; 20]);
    table.insert(DhtNode::new([1u8; 20], make_addr(6881)));

    let good = table.collect_good_nodes();
    assert_eq!(good.len(), 1);
}

#[test]
fn test_no_self_insert() {
    let self_id = [0x42u8; 20];
    let mut table = RoutingTable::new(self_id);

    // Try to insert our own ID — should be rejected.
    table.insert(DhtNode::new(self_id, make_addr(6881)));
    assert_eq!(table.total_node_count(), 0);
}

#[test]
fn test_cache_node_on_full_bucket() {
    // Use a local_id in the upper half so that after splits, the lower bucket
    // cannot split further (local_id not in range), forcing caching.
    let self_id = [0xFFu8; 20];
    let mut table = RoutingTable::new(self_id);

    // Fill bucket with K good nodes in the lower half (IDs 1..=8).
    // After splits, the lower bucket will be full and split_allowed=false
    // because local_id [0xFF..] is in the upper bucket's range.
    for i in 1..=super::super::bucket::K as u8 {
        let mut id = [0u8; 20];
        id[0] = i; // IDs in lower half
        table.insert(DhtNode::new(id, make_addr(6881 + i as u16)));
    }

    // Add more lower-half nodes to fill the lower bucket after splits.
    for i in 9..=20u8 {
        let mut id = [0u8; 20];
        id[0] = i; // Still in lower half
        table.insert(DhtNode::new(id, make_addr(7000 + i as u16)));
    }

    // Check that at least one bucket has cached nodes.
    let buckets = table.get_all_buckets();
    let has_cached = buckets.iter().any(|b| !b.cached_nodes().is_empty());
    assert!(
        has_cached,
        "Expected at least one bucket to have cached nodes"
    );
}

#[test]
fn full_non_local_bucket_caches_only_verified_replacements() {
    let self_id = [0xFF; 20];
    let mut table = RoutingTable::new(self_id);
    for i in 1..=super::super::bucket::K as u8 {
        table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
    }

    let candidate_id = [0x40; 20];
    table.insert(DhtNode::unverified(candidate_id, make_addr(6990)));
    assert_eq!(table.num_buckets(), 2);

    let bucket = table.get_bucket_for(&candidate_id);
    assert!(
        !bucket.is_in_range(&self_id),
        "candidate must be in a non-local bucket"
    );
    assert_eq!(bucket.count_node(), super::super::bucket::K);
    assert!(bucket.nodes().iter().all(|node| node.id() != &candidate_id));
    assert!(
        bucket
            .cached_nodes()
            .iter()
            .all(|node| node.id() != &candidate_id),
        "unverified candidates must not enter the replacement cache"
    );

    let verified_id = [0x41; 20];
    table.insert(DhtNode::new(verified_id, make_addr(6991)));
    assert!(
        table
            .get_bucket_for(&verified_id)
            .cached_nodes()
            .iter()
            .any(|node| node.id() == &verified_id),
        "verified candidates must enter the replacement cache"
    );
}

#[test]
fn unverified_candidate_cannot_replace_bad_lru_in_full_non_local_bucket() {
    let self_id = [0xFF; 20];
    let mut table = RoutingTable::new(self_id);
    for i in 1..=super::super::bucket::K as u8 {
        table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
    }

    let failed_id = [1; 20];
    for _ in 0..5 {
        assert!(table.mark_bad(&failed_id));
    }

    let unverified_id = [0x40; 20];
    table.insert(DhtNode::unverified(unverified_id, make_addr(6990)));

    let bucket = table.get_bucket_for(&unverified_id);
    assert_eq!(bucket.count_node(), super::super::bucket::K);
    assert!(
        bucket.nodes().iter().any(|node| node.id() == &failed_id),
        "insertion must not evict a bad node in favor of an unverified candidate"
    );
    assert!(
        bucket
            .nodes()
            .iter()
            .all(|node| node.id() != &unverified_id),
        "an unverified candidate must not enter a full non-local bucket"
    );
    assert!(
        bucket
            .cached_nodes()
            .iter()
            .all(|node| node.id() != &unverified_id),
        "an unverified candidate must not enter the replacement cache"
    );

    assert_eq!(table.evict_bad_nodes(), 1);
    let bucket = table.get_bucket_for(&unverified_id);
    assert_eq!(bucket.count_node(), super::super::bucket::K - 1);
    assert!(
        bucket
            .nodes()
            .iter()
            .all(|node| node.id() != &unverified_id),
        "bad-node eviction must not promote the rejected unverified candidate"
    );
}

#[test]
fn test_evict_bad_nodes() {
    let mut table = RoutingTable::new([0u8; 20]);
    for i in 1..=super::super::bucket::K as u8 {
        table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
    }
    let lower_bad_id = [1; 20];
    let upper_bad_id = [0x80; 20];
    table.insert(DhtNode::new(upper_bad_id, make_addr(6999)));
    assert_eq!(table.num_buckets(), 2);
    for _ in 0..5 {
        assert!(table.mark_bad(&lower_bad_id));
        assert!(table.mark_bad(&upper_bad_id));
    }

    assert_eq!(table.evict_bad_nodes(), 2);
    assert_eq!(table.total_node_count(), super::super::bucket::K - 1);
}

#[test]
fn evicting_bad_node_promotes_the_newest_verified_cached_replacement() {
    let mut table = RoutingTable::new([0xFF; 20]);
    for i in 1..=super::super::bucket::K as u8 {
        table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
    }

    let older_candidate = [0x40; 20];
    let replacement_id = [0x41; 20];
    table.insert(DhtNode::new(older_candidate, make_addr(6990)));
    table.insert(DhtNode::new(replacement_id, make_addr(6991)));
    let unverified_id = [0x3F; 20];
    table.insert(DhtNode::unverified(unverified_id, make_addr(6989)));

    let failed_id = [1; 20];
    for _ in 0..5 {
        assert!(table.mark_bad(&failed_id));
    }

    assert_eq!(table.evict_bad_nodes(), 1);
    let bucket = table.get_bucket_for(&replacement_id);
    assert_eq!(bucket.count_node(), super::super::bucket::K);
    assert!(
        bucket
            .nodes()
            .iter()
            .any(|node| node.id() == &replacement_id)
    );
    assert!(
        bucket
            .cached_nodes()
            .iter()
            .any(|node| node.id() == &older_candidate),
        "remaining replacement cache: {:?}",
        bucket
            .cached_nodes()
            .iter()
            .map(DhtNode::id)
            .collect::<Vec<_>>()
    );
    assert!(
        bucket
            .nodes()
            .iter()
            .chain(bucket.cached_nodes())
            .all(|node| node.id() != &unverified_id),
        "unverified candidate must not be promoted or retained in the replacement cache"
    );
}
