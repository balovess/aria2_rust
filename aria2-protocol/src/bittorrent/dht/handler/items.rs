use std::net::SocketAddr;

use super::super::message::{DhtMessage, DhtMessageBuilder};
use super::super::modern::{MutableValue, StoredItem};
use super::super::routing_table::RoutingTable;
use super::super::store::{DhtItemStore, StoreError};
use super::super::token_tracker::TokenTracker;
use super::{DhtQueryHandler, K};
use crate::bittorrent::bencode::codec::BencodeValue;

impl DhtQueryHandler {
    pub(super) fn handle_get_item(
        &self,
        tx: &[u8],
        from: SocketAddr,
        query: &DhtMessage,
        routing_table: &RoutingTable,
        token_tracker: &TokenTracker,
        item_store: &DhtItemStore,
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
        let token = token_tracker.generate_token(&target, &from);
        let mut result = std::collections::BTreeMap::new();
        result.insert(b"id".to_vec(), BencodeValue::Bytes(self.self_id.to_vec()));
        result.insert(b"token".to_vec(), BencodeValue::Bytes(token.into_bytes()));
        if let Some(item) = item_store.get(&target) {
            let include = query
                .a
                .as_ref()
                .and_then(|args| args.dict_get(b"seq"))
                .and_then(|value| value.as_int())
                .is_none_or(|seq| match &item {
                    StoredItem::Mutable { item, .. } => item.sequence > seq,
                    StoredItem::Immutable { .. } => true,
                });
            if include {
                match item {
                    StoredItem::Immutable { value, .. } => {
                        result.insert(b"v".to_vec(), value);
                    }
                    StoredItem::Mutable { item, .. } => {
                        result.insert(b"k".to_vec(), BencodeValue::Bytes(item.public_key.to_vec()));
                        result.insert(
                            b"sig".to_vec(),
                            BencodeValue::Bytes(item.signature.to_vec()),
                        );
                        result.insert(b"seq".to_vec(), BencodeValue::Int(item.sequence));
                        result.insert(b"v".to_vec(), item.value);
                        if let Some(salt) = item.salt {
                            result.insert(b"salt".to_vec(), BencodeValue::Bytes(salt));
                        }
                    }
                }
            }
        }
        let nodes = Self::encode_compact_nodes(&routing_table.find_closest(&target, K));
        result.insert(b"nodes".to_vec(), BencodeValue::Bytes(nodes));
        Some(DhtMessage::new_response(
            tx.to_vec(),
            BencodeValue::Dict(result),
        ))
    }

    #[allow(clippy::needless_return)]
    pub(super) fn handle_put_item(
        &self,
        tx: &[u8],
        from: SocketAddr,
        query: &DhtMessage,
        token_tracker: &TokenTracker,
        item_store: &DhtItemStore,
    ) -> Option<DhtMessage> {
        let args = match query.a.as_ref() {
            Some(args) => args,
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        let token = match args.dict_get(b"token").and_then(|v| v.as_bytes()) {
            Some(token) => token,
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        let value = match args.dict_get(b"v") {
            Some(value) => value.clone(),
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        let has_mutable_fields = args.dict_get(b"k").is_some()
            || args.dict_get(b"seq").is_some()
            || args.dict_get(b"sig").is_some()
            || args.dict_get(b"salt").is_some()
            || args.dict_get(b"cas").is_some();
        if let (Some(k), Some(seq), Some(sig)) = (
            args.dict_get(b"k").and_then(|v| v.as_bytes()),
            args.dict_get(b"seq").and_then(|v| v.as_int()),
            args.dict_get(b"sig").and_then(|v| v.as_bytes()),
        ) {
            let public_key: [u8; 32] = match k.try_into() {
                Ok(key) => key,
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        206,
                        "invalid signature",
                    ));
                }
            };
            let signature: [u8; 64] = match sig.try_into() {
                Ok(signature) => signature,
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        206,
                        "invalid signature",
                    ));
                }
            };
            let salt = match args.dict_get(b"salt") {
                Some(value) => match value.as_bytes() {
                    Some(salt) => Some(salt.to_vec()),
                    None => {
                        return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
                    }
                },
                None => None,
            };
            let target = StoredItem::mutable_target(&public_key, salt.as_deref());
            if !token_tracker.validate_token_bytes(token, &target, &from) {
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
            let item = MutableValue {
                public_key,
                signature,
                sequence: seq,
                salt,
                value,
            };
            match item_store.put_mutable(item, args.dict_get(b"cas").and_then(|v| v.as_int())) {
                Ok(_) => {
                    return Some(DhtMessage::new_response(
                        tx.to_vec(),
                        BencodeValue::Dict(std::collections::BTreeMap::from([(
                            b"id".to_vec(),
                            BencodeValue::Bytes(self.self_id.to_vec()),
                        )])),
                    ));
                }
                Err(StoreError::CasMismatch) => {
                    return Some(DhtMessageBuilder::error_response(tx, 301, "CAS mismatch"));
                }
                Err(StoreError::SequenceTooLow) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        302,
                        "sequence number less than current",
                    ));
                }
                Err(StoreError::ValueTooLarge) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        205,
                        "message too big",
                    ));
                }
                Err(StoreError::SaltTooLarge) => {
                    return Some(DhtMessageBuilder::error_response(tx, 207, "salt too big"));
                }
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        206,
                        "invalid signature",
                    ));
                }
            }
        } else if has_mutable_fields {
            return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
        } else {
            let target = StoredItem::immutable_target(&value);
            if !token_tracker.validate_token_bytes(token, &target, &from) {
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
            match item_store.put_immutable(value) {
                Ok(_) => Some(DhtMessage::new_response(
                    tx.to_vec(),
                    BencodeValue::Dict(std::collections::BTreeMap::from([(
                        b"id".to_vec(),
                        BencodeValue::Bytes(self.self_id.to_vec()),
                    )])),
                )),
                Err(StoreError::ValueTooLarge) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        205,
                        "message too big",
                    ));
                }
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
                }
            }
        }
    }
}
