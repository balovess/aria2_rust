use std::collections::BTreeMap;

use super::*;
use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::dht::modern::{MutableValue, StoredItem};
use crate::bittorrent::dht::routing_table::RoutingTable;

fn make_handler() -> DhtQueryHandler {
    DhtQueryHandler::new([0xAAu8; 20])
}

fn make_routing_table() -> RoutingTable {
    let mut rt = RoutingTable::new([0xAAu8; 20]);
    // Add some nodes
    for i in 0..8u8 {
        let node = DhtNode::new([i; 20], format!("10.0.0.{}:6881", i).parse().unwrap());
        rt.insert(node);
    }
    rt
}

mod announce_peer;
mod basic_queries;
mod items;
mod peer_responses;
