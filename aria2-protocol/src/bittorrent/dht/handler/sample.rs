use super::super::message::{DhtMessage, DhtMessageBuilder};
use super::super::peer_storage::DhtPeerStorage;
use super::super::routing_table::RoutingTable;
use super::DhtQueryHandler;
use super::K;
use crate::bittorrent::bencode::codec::BencodeValue;

impl DhtQueryHandler {
    pub(super) fn handle_sample_infohashes(
        &self,
        tx: &[u8],
        query: &DhtMessage,
        routing_table: &RoutingTable,
        peer_storage: &DhtPeerStorage,
    ) -> Option<DhtMessage> {
        let target: [u8; 20] = match query
            .a
            .as_ref()
            .and_then(|args| args.dict_get(b"target"))
            .and_then(|value| value.as_bytes())
            .and_then(|bytes| bytes.try_into().ok())
        {
            Some(target) => target,
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        // BEP 51 samples the torrent swarm keyspace. BEP 44 item targets are
        // a separate keyspace and must not be advertised as torrent hashes.
        let (num, info_hashes) = peer_storage.sample_info_hashes_with_count(32);
        let mut result = std::collections::BTreeMap::new();
        result.insert(b"id".to_vec(), BencodeValue::Bytes(self.self_id.to_vec()));
        result.insert(b"interval".to_vec(), BencodeValue::Int(900));
        result.insert(b"num".to_vec(), BencodeValue::Int(num as i64));
        result.insert(
            b"samples".to_vec(),
            BencodeValue::Bytes(info_hashes.into_iter().flatten().collect()),
        );
        let closest = routing_table.find_closest(&target, K);
        let nodes = Self::encode_compact_nodes(&closest);
        let nodes6 = Self::encode_compact_nodes6(&closest);
        result.insert(b"nodes".to_vec(), BencodeValue::Bytes(nodes));
        if !nodes6.is_empty() {
            result.insert(b"nodes6".to_vec(), BencodeValue::Bytes(nodes6));
        }
        Some(DhtMessage::new_response(
            tx.to_vec(),
            BencodeValue::Dict(result),
        ))
    }
}
