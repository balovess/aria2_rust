use super::*;

#[test]
fn test_put_rejects_non_bytes_salt() {
    use ed25519_dalek::{Signer, SigningKey};

    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let store = DhtItemStore::default();
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let mut item = MutableValue {
        public_key: key.verifying_key().to_bytes(),
        signature: [0u8; 64],
        sequence: 1,
        salt: None,
        value: BencodeValue::Bytes(b"value".to_vec()),
    };
    item.signature = key.sign(&item.signed_payload()).to_bytes();
    let target = StoredItem::mutable_target(&item.public_key, None);
    let from: SocketAddr = "10.0.0.1:6881".parse().unwrap();
    let token = tt.generate_token(&target, &from);
    let mut query =
        super::super::super::modern::put_query(7, &[0xBB; 20], token.as_bytes(), &item, None);
    if let Some(BencodeValue::Dict(args)) = query.a.as_mut() {
        args.insert(b"salt".to_vec(), BencodeValue::Int(1));
    } else {
        panic!("PUT query must have a dictionary of arguments");
    }

    let result = handler.handle_query(&query, from, &rt, &tt, &ps, Some(&store));
    assert!(result.response.is_some_and(|response| response.is_error()));
    assert!(store.get(&target).is_none());
}

#[test]
fn test_put_rejects_non_integer_cas_instead_of_ignoring_it() {
    use ed25519_dalek::{Signer, SigningKey};

    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let store = DhtItemStore::default();
    let key = SigningKey::from_bytes(&[0x32; 32]);
    let mut item = MutableValue {
        public_key: key.verifying_key().to_bytes(),
        signature: [0; 64],
        sequence: 1,
        salt: None,
        value: BencodeValue::Bytes(b"value".to_vec()),
    };
    item.signature = key.sign(&item.signed_payload()).to_bytes();
    let target = StoredItem::mutable_target(&item.public_key, None);
    let from: SocketAddr = "10.0.0.1:6881".parse().unwrap();
    let token = tt.generate_token(&target, &from);
    let mut query =
        super::super::super::modern::put_query(8, &[0xBC; 20], token.as_bytes(), &item, None);
    let Some(BencodeValue::Dict(args)) = query.a.as_mut() else {
        panic!("PUT query must have a dictionary of arguments");
    };
    args.insert(b"cas".to_vec(), BencodeValue::Bytes(b"0".to_vec()));

    let result = handler.handle_query(&query, from, &rt, &tt, &ps, Some(&store));
    assert!(result.response.is_some_and(|response| response.is_error()));
    assert!(store.get(&target).is_none());
}

#[test]
fn test_get_rejects_non_integer_sequence_filter() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let store = DhtItemStore::default();
    let target = [0x41; 20];
    let mut query = super::super::super::modern::get_query(9, &[0xBD; 20], &target, None);
    let Some(BencodeValue::Dict(args)) = query.a.as_mut() else {
        panic!("GET query must have a dictionary of arguments");
    };
    args.insert(b"seq".to_vec(), BencodeValue::Bytes(b"4".to_vec()));
    let from: SocketAddr = "10.0.0.1:6881".parse().unwrap();

    let result = handler.handle_query(&query, from, &rt, &tt, &ps, Some(&store));
    assert!(result.response.is_some_and(|response| response.is_error()));
}
