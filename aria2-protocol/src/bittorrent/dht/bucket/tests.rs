use super::*;
use std::net::SocketAddr;

fn make_local_node() -> DhtNode {
    DhtNode::new([0u8; 20], "127.0.0.1:6881".parse::<SocketAddr>().unwrap())
}

#[test]
fn test_bucket_creation_full_range() {
    let local = make_local_node();
    let bucket = Bucket::new(&local);
    assert_eq!(bucket.prefix_length(), 0);
    assert_eq!(bucket.min_id(), &[0u8; 20]);
    assert_eq!(bucket.max_id(), &[0xFFu8; 20]);
    assert_eq!(bucket.count_node(), 0);
    assert!(!bucket.is_full());
}

#[test]
fn test_bucket_is_in_range() {
    let local = make_local_node();
    let bucket = Bucket::new(&local);
    assert!(bucket.is_in_range(&[0u8; 20]));
    assert!(bucket.is_in_range(&[0xFFu8; 20]));
    assert!(bucket.is_in_range(&[0x80u8; 20]));
}

#[test]
fn test_add_node() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);
    let node = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
    assert!(bucket.add_node(node));
    assert_eq!(bucket.count_node(), 1);
}

#[test]
fn test_add_node_full_bucket_replaces_bad() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);

    // Fill the bucket with bad nodes.
    for i in 0..K {
        let mut node = DhtNode::new(
            [i as u8; 20],
            format!("127.0.0.1:{}", 6882 + i).parse().unwrap(),
        );
        for _ in 0..5 {
            node.record_failure();
        }
        bucket.add_node(node);
    }
    assert!(bucket.is_full());

    // The first node (id=[0]) should be bad and LRU.
    let new_node = DhtNode::new([0xFFu8; 20], "127.0.0.1:9999".parse().unwrap());
    assert!(bucket.add_node(new_node));
    assert_eq!(bucket.count_node(), K);
}

#[test]
fn test_add_node_full_bucket_rejects() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);

    // Fill with good nodes.
    for i in 0..K {
        let node = DhtNode::new(
            [(i + 1) as u8; 20],
            format!("127.0.0.1:{}", 6882 + i).parse().unwrap(),
        );
        bucket.add_node(node);
    }
    assert!(bucket.is_full());

    // All nodes are good; new node should be rejected.
    let new_node = DhtNode::new([0xFFu8; 20], "127.0.0.1:9999".parse().unwrap());
    assert!(!bucket.add_node(new_node));
}

#[test]
fn test_split_allowed() {
    let local = make_local_node();
    let bucket = Bucket::new(&local);
    // Full-range bucket containing local node ID — should be splittable.
    assert!(bucket.split_allowed());
}

#[test]
fn test_split_not_allowed_if_local_id_out_of_range() {
    let local = DhtNode::new([0xFFu8; 20], "127.0.0.1:6881".parse().unwrap());
    let bucket = Bucket::new_for_range(
        1,
        [0u8; 20],    // min
        [0x7Fu8; 20], // max (first bit = 0)
        local.id,
    );
    // Local ID [0xFF..] is not in range [0x00.., 0x7F..]
    assert!(!bucket.split_allowed());
}

#[test]
fn test_split() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);

    // Add nodes to the left half (first bit = 0).
    for i in 0..4u8 {
        let node = DhtNode::new(
            [i; 20],
            format!("127.0.0.1:{}", 6882 + i as u16).parse().unwrap(),
        );
        bucket.add_node(node);
    }

    // Add nodes to the right half (first bit = 1).
    for i in 0..4u8 {
        let mut id = [0u8; 20];
        id[0] = 0x80 | i;
        let node = DhtNode::new(
            id,
            format!("127.0.0.2:{}", 6882 + i as u16).parse().unwrap(),
        );
        bucket.add_node(node);
    }

    assert_eq!(bucket.count_node(), 8);

    // Split the bucket.
    let right_bucket = bucket.split();

    // Left bucket should have nodes with first bit = 0.
    assert_eq!(bucket.prefix_length(), 1);
    assert_eq!(bucket.count_node(), 4);

    // Right bucket should have nodes with first bit = 1.
    assert_eq!(right_bucket.prefix_length(), 1);
    assert_eq!(right_bucket.count_node(), 4);
}

#[test]
fn test_cache_node() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);
    let node = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
    bucket.cache_node(node);
    assert_eq!(bucket.cached_nodes().len(), 1);
}

#[test]
fn test_cache_node_limit() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);
    for i in 0..5u8 {
        let node = DhtNode::new(
            [i; 20],
            format!("127.0.0.1:{}", 6882 + i as u16).parse().unwrap(),
        );
        bucket.cache_node(node);
    }
    assert_eq!(bucket.cached_nodes().len(), CACHE_SIZE);
}

#[test]
fn test_drop_node_promotes_cached() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);

    // Add a node.
    let node_id = [1u8; 20];
    let node = DhtNode::new(node_id, "127.0.0.1:6882".parse().unwrap());
    bucket.add_node(node);

    // Cache a replacement.
    let replacement = DhtNode::new([2u8; 20], "127.0.0.1:6883".parse().unwrap());
    bucket.cache_node(replacement);

    // Drop the original node.
    assert!(bucket.drop_node(&node_id));
    assert_eq!(bucket.count_node(), 1);
    // The replacement should now be in the main node list.
    assert!(bucket.nodes().iter().any(|n| n.id == [2u8; 20]));
}

#[test]
fn test_replace_node_consumes_cached_candidate() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);
    let node_id = [1u8; 20];
    let replacement_id = [2u8; 20];

    bucket.add_node(DhtNode::new(node_id, "127.0.0.1:6882".parse().unwrap()));
    bucket.cache_node(DhtNode::new(
        replacement_id,
        "127.0.0.1:6883".parse().unwrap(),
    ));

    assert!(bucket.remove_cached_node(&replacement_id));
    assert!(bucket.replace_node(
        &node_id,
        DhtNode::new(replacement_id, "127.0.0.1:6883".parse().unwrap())
    ));
    assert!(!bucket.nodes().iter().any(|node| node.id == node_id));
    assert!(bucket.nodes().iter().any(|node| node.id == replacement_id));
    assert!(bucket.cached_nodes().is_empty());
}

#[test]
fn test_get_random_node_id() {
    let local = make_local_node();
    let bucket = Bucket::new(&local);
    let id = bucket.get_random_node_id();
    // For a full-range bucket, the ID should be within range.
    assert!(bucket.is_in_range(&id));
}

#[test]
fn test_needs_refresh_empty() {
    let local = make_local_node();
    let bucket = Bucket::new(&local);
    // Empty bucket needs refresh.
    assert!(bucket.needs_refresh());
}

#[test]
fn test_contains_questionable_node() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);

    // Fresh node is not questionable.
    let node = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
    bucket.add_node(node);
    assert!(!bucket.contains_questionable_node());
}

#[test]
fn test_move_to_tail() {
    let local = make_local_node();
    let mut bucket = Bucket::new(&local);

    let n1 = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
    let n2 = DhtNode::new([2u8; 20], "127.0.0.1:6883".parse().unwrap());
    bucket.add_node(n1);
    bucket.add_node(n2);

    // Move n1 to tail.
    bucket.move_to_tail(&[1u8; 20]);
    assert_eq!(bucket.nodes()[1].id, [1u8; 20]);
}
