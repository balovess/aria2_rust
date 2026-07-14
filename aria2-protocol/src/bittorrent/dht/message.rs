use crate::bittorrent::bencode::codec::BencodeValue;

#[derive(Debug, Clone)]
pub enum DhtMessageType {
    Query,
    Response,
    Error,
}

#[derive(Debug, Clone)]
pub struct DhtQueryMethod(pub String);

impl DhtQueryMethod {
    pub const PING: &'static str = "ping";
    pub const FIND_NODE: &'static str = "find_node";
    pub const GET_PEERS: &'static str = "get_peers";
    pub const ANNOUNCE_PEER: &'static str = "announce_peer";
}

#[derive(Debug, Clone)]
pub struct DhtMessage {
    pub t: Vec<u8>,
    pub y: DhtMessageType,
    pub q: Option<DhtQueryMethod>,
    pub a: Option<BencodeValue>,
    pub r: Option<BencodeValue>,
    pub e: Option<(i64, String)>,
}

impl DhtMessage {
    pub fn new_query(tx_id: u32, method: &str, args: BencodeValue) -> Self {
        Self {
            t: tx_id.to_be_bytes().to_vec(),
            y: DhtMessageType::Query,
            q: Some(DhtQueryMethod(method.to_string())),
            a: Some(args),
            r: None,
            e: None,
        }
    }

    pub fn new_response(tx_id: Vec<u8>, result: BencodeValue) -> Self {
        Self {
            t: tx_id,
            y: DhtMessageType::Response,
            q: None,
            a: None,
            r: Some(result),
            e: None,
        }
    }

    pub fn new_error(tx_id: Vec<u8>, code: i64, msg: &str) -> Self {
        Self {
            t: tx_id,
            y: DhtMessageType::Error,
            q: None,
            a: None,
            r: None,
            e: Some((code, msg.to_string())),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        use std::collections::BTreeMap;
        let mut dict = BTreeMap::new();

        dict.insert(b"t".to_vec(), BencodeValue::Bytes(self.t.clone()));
        dict.insert(
            b"y".to_vec(),
            BencodeValue::Bytes(match self.y {
                DhtMessageType::Query => b"q".to_vec(),
                DhtMessageType::Response => b"r".to_vec(),
                DhtMessageType::Error => b"e".to_vec(),
            }),
        );

        match &self.y {
            DhtMessageType::Query => {
                if let Some(ref method) = self.q {
                    dict.insert(
                        b"q".to_vec(),
                        BencodeValue::Bytes(method.0.clone().into_bytes()),
                    );
                }
                if let Some(ref args) = self.a {
                    dict.insert(b"a".to_vec(), args.clone());
                }
            }
            DhtMessageType::Response => {
                if let Some(ref result) = self.r {
                    dict.insert(b"r".to_vec(), result.clone());
                }
            }
            DhtMessageType::Error => {
                if let Some((code, msg)) = &self.e {
                    dict.insert(
                        b"e".to_vec(),
                        BencodeValue::List(vec![
                            BencodeValue::Int(*code),
                            BencodeValue::Bytes(msg.clone().into_bytes()),
                        ]),
                    );
                }
            }
        }

        Ok(BencodeValue::Dict(dict).encode())
    }

    pub fn decode(data: &[u8]) -> Result<Self, String> {
        let (root, _) = BencodeValue::decode(data)?;

        let t = root
            .dict_get(b"t")
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec())
            .ok_or("缺少t字段")?;

        let y_bytes = root
            .dict_get(b"y")
            .and_then(|v| v.as_bytes())
            .ok_or("缺少y字段")?;

        let y = match y_bytes.first() {
            Some(b'q') => DhtMessageType::Query,
            Some(b'r') => DhtMessageType::Response,
            Some(b'e') => DhtMessageType::Error,
            _ => return Err(format!("无效的y值: {:?}", y_bytes)),
        };

        match y {
            DhtMessageType::Query => {
                let q_str = root.dict_get_str("q").ok_or("缺少q字段")?;
                let args = root.dict_get(b"a").cloned();
                Ok(Self {
                    t,
                    y,
                    q: Some(DhtQueryMethod(q_str.to_string())),
                    a: args,
                    r: None,
                    e: None,
                })
            }
            DhtMessageType::Response => {
                let r = root.dict_get(b"r").cloned();
                Ok(Self {
                    t,
                    y,
                    q: None,
                    a: None,
                    r,
                    e: None,
                })
            }
            DhtMessageType::Error => {
                let err_val = root
                    .dict_get(b"e")
                    .and_then(|v| v.as_list())
                    .ok_or("缺少e字段")?;
                if err_val.len() < 2 {
                    return Err("error格式错误".to_string());
                }
                let code = err_val[0].as_int().unwrap_or(201);
                let msg = err_val[1].as_str().unwrap_or("unknown error");
                Ok(Self {
                    t,
                    y,
                    q: None,
                    a: None,
                    r: None,
                    e: Some((code, msg.to_string())),
                })
            }
        }
    }

    pub fn is_query(&self) -> bool {
        matches!(self.y, DhtMessageType::Query)
    }
    pub fn is_response(&self) -> bool {
        matches!(self.y, DhtMessageType::Response)
    }
    pub fn is_error(&self) -> bool {
        matches!(self.y, DhtMessageType::Error)
    }
}

/// Encode a `SocketAddr` into BEP 0005 compact peer format.
///
/// - IPv4: 6 bytes (4 bytes IP + 2 bytes port, big-endian)
/// - IPv6: 18 bytes (16 bytes IP + 2 bytes port, big-endian)
///
/// The output is directly consumable by
/// [`crate::bittorrent::dht::client::extract_compact_peers_from_response`].
pub fn encode_compact_peer(addr: std::net::SocketAddr) -> Vec<u8> {
    match addr {
        std::net::SocketAddr::V4(v4) => {
            let mut buf = Vec::with_capacity(6);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
            buf
        }
        std::net::SocketAddr::V6(v6) => {
            let mut buf = Vec::with_capacity(18);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
            buf
        }
    }
}

pub struct DhtMessageBuilder;

impl DhtMessageBuilder {
    pub fn ping(transaction_id: u32, sender_id: &[u8; 20]) -> DhtMessage {
        let mut args_dict = std::collections::BTreeMap::new();
        args_dict.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
        DhtMessage::new_query(
            transaction_id,
            DhtQueryMethod::PING,
            BencodeValue::Dict(args_dict),
        )
    }

    pub fn find_node(transaction_id: u32, sender_id: &[u8; 20], target: &[u8; 20]) -> DhtMessage {
        let mut args_dict = std::collections::BTreeMap::new();
        args_dict.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
        args_dict.insert(b"target".to_vec(), BencodeValue::Bytes(target.to_vec()));
        DhtMessage::new_query(
            transaction_id,
            DhtQueryMethod::FIND_NODE,
            BencodeValue::Dict(args_dict),
        )
    }

    pub fn get_peers(
        transaction_id: u32,
        sender_id: &[u8; 20],
        info_hash: &[u8; 20],
    ) -> DhtMessage {
        let mut args_dict = std::collections::BTreeMap::new();
        args_dict.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
        args_dict.insert(
            b"info_hash".to_vec(),
            BencodeValue::Bytes(info_hash.to_vec()),
        );
        DhtMessage::new_query(
            transaction_id,
            DhtQueryMethod::GET_PEERS,
            BencodeValue::Dict(args_dict),
        )
    }

    pub fn announce_peer(
        transaction_id: u32,
        sender_id: &[u8; 20],
        info_hash: &[u8; 20],
        port: u16,
        token: &str,
    ) -> DhtMessage {
        let mut args_dict = std::collections::BTreeMap::new();
        args_dict.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
        args_dict.insert(
            b"info_hash".to_vec(),
            BencodeValue::Bytes(info_hash.to_vec()),
        );
        args_dict.insert(b"port".to_vec(), BencodeValue::Int(port as i64));
        args_dict.insert(
            b"token".to_vec(),
            BencodeValue::Bytes(token.as_bytes().to_vec()),
        );
        DhtMessage::new_query(
            transaction_id,
            DhtQueryMethod::ANNOUNCE_PEER,
            BencodeValue::Dict(args_dict),
        )
    }

    // ==================== Response Builders ====================

    /// Build a ping response: `{"t":tx,"y":"r","r":{"id":self_id}}`.
    ///
    /// The `tx` parameter is the transaction ID from the original ping query
    /// and is echoed back verbatim per BEP 0005.
    pub fn ping_response(tx: &[u8], self_id: &[u8; 20]) -> DhtMessage {
        let mut r_dict = std::collections::BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(self_id.to_vec()));
        DhtMessage::new_response(tx.to_vec(), BencodeValue::Dict(r_dict))
    }

    /// Build a find_node response:
    /// `{"t":tx,"y":"r","r":{"id":self_id,"nodes":compact_nodes}}`.
    ///
    /// `compact_nodes` is a concatenation of 26-byte compact node entries
    /// (20 bytes node ID + 6 bytes IPv4 compact addr) per BEP 0005.
    pub fn find_node_response(tx: &[u8], self_id: &[u8; 20], compact_nodes: &[u8]) -> DhtMessage {
        let mut r_dict = std::collections::BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(self_id.to_vec()));
        r_dict.insert(
            b"nodes".to_vec(),
            BencodeValue::Bytes(compact_nodes.to_vec()),
        );
        DhtMessage::new_response(tx.to_vec(), BencodeValue::Dict(r_dict))
    }

    /// Build a get_peers response carrying known peers:
    /// `{"t":tx,"y":"r","r":{"id":self_id,"token":token,"values":[...]}}`.
    ///
    /// Each peer in `peers` is encoded via [`encode_compact_peer`]
    /// (6 bytes for IPv4, 18 bytes for IPv6).
    pub fn get_peers_response_with_peers(
        tx: &[u8],
        self_id: &[u8; 20],
        token: &[u8],
        peers: &[std::net::SocketAddr],
    ) -> DhtMessage {
        let mut r_dict = std::collections::BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(self_id.to_vec()));
        r_dict.insert(b"token".to_vec(), BencodeValue::Bytes(token.to_vec()));
        let values: Vec<BencodeValue> = peers
            .iter()
            .map(|p| BencodeValue::Bytes(encode_compact_peer(*p)))
            .collect();
        r_dict.insert(b"values".to_vec(), BencodeValue::List(values));
        DhtMessage::new_response(tx.to_vec(), BencodeValue::Dict(r_dict))
    }

    /// Build a get_peers response carrying closest nodes (no peers known):
    /// `{"t":tx,"y":"r","r":{"id":self_id,"token":token,"nodes":compact_nodes}}`.
    pub fn get_peers_response_with_nodes(
        tx: &[u8],
        self_id: &[u8; 20],
        token: &[u8],
        compact_nodes: &[u8],
    ) -> DhtMessage {
        let mut r_dict = std::collections::BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(self_id.to_vec()));
        r_dict.insert(b"token".to_vec(), BencodeValue::Bytes(token.to_vec()));
        r_dict.insert(
            b"nodes".to_vec(),
            BencodeValue::Bytes(compact_nodes.to_vec()),
        );
        DhtMessage::new_response(tx.to_vec(), BencodeValue::Dict(r_dict))
    }

    /// Build an announce_peer response: `{"t":tx,"y":"r","r":{"id":self_id}}`.
    pub fn announce_peer_response(tx: &[u8], self_id: &[u8; 20]) -> DhtMessage {
        let mut r_dict = std::collections::BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(self_id.to_vec()));
        DhtMessage::new_response(tx.to_vec(), BencodeValue::Dict(r_dict))
    }

    /// Build a DHT error response: `{"t":tx,"y":"e","e":[code,message]}`.
    pub fn error_response(tx: &[u8], code: i64, message: &str) -> DhtMessage {
        DhtMessage::new_error(tx.to_vec(), code, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ping_encode_decode_roundtrip() {
        let id = [1u8; 20];
        let msg = DhtMessageBuilder::ping(1234, &id);
        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_query());
        assert_eq!(&decoded.t, &msg.t);
    }

    #[test]
    fn test_find_node_message() {
        let sender = [1u8; 20];
        let target = [2u8; 20];
        let msg = DhtMessageBuilder::find_node(5678, &sender, &target);
        assert!(msg.is_query());

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.q.as_ref().unwrap().0, "find_node");
    }

    #[test]
    fn test_error_message() {
        let msg = DhtMessage::new_error(vec![0xAA, 0xBB], 203, "Server Error");
        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();
        assert!(decoded.is_error());
        assert_eq!(decoded.e, Some((203, "Server Error".to_string())));
    }

    #[test]
    fn test_response_message() {
        let mut result = std::collections::BTreeMap::new();
        result.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0u8; 20]));
        let msg = DhtMessage::new_response(vec![0x01], BencodeValue::Dict(result));
        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();
        assert!(decoded.is_response());
    }

    // ==================== encode_compact_peer tests ====================

    #[test]
    fn test_encode_compact_peer_ipv4() {
        let addr: std::net::SocketAddr = "192.168.1.100:8080".parse().unwrap();
        let bytes = encode_compact_peer(addr);
        assert_eq!(bytes.len(), 6);
        assert_eq!(&bytes[0..4], &[192, 168, 1, 100]);
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 8080);
    }

    #[test]
    fn test_encode_compact_peer_ipv6() {
        let addr: std::net::SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let bytes = encode_compact_peer(addr);
        assert_eq!(bytes.len(), 18);
        // First 16 bytes are the IPv6 address octets
        let expected_octets: [u8; 16] =
            std::net::Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1).octets();
        assert_eq!(&bytes[0..16], &expected_octets[..]);
        assert_eq!(u16::from_be_bytes([bytes[16], bytes[17]]), 443);
    }

    // ==================== Response builder tests ====================

    #[test]
    fn test_ping_response_encode_decode() {
        let tx = [0x01u8, 0x02, 0x03, 0x04];
        let self_id = [0xAAu8; 20];
        let msg = DhtMessageBuilder::ping_response(&tx, &self_id);

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_response());
        assert_eq!(decoded.t, tx.to_vec());

        let r = decoded.r.as_ref().expect("response must have r field");
        let id_bytes = r
            .dict_get(b"id")
            .and_then(|v| v.as_bytes())
            .expect("missing r.id");
        assert_eq!(id_bytes, &self_id[..]);
    }

    #[test]
    fn test_find_node_response_encode_decode() {
        let tx = [0x11u8, 0x22];
        let self_id = [0xBBu8; 20];

        // Build 2 compact nodes (26 bytes each: 20 ID + 4 IP + 2 port)
        let mut compact_nodes = Vec::new();
        // Node 1: id=0x01.., IP 192.168.1.1:8080
        compact_nodes.extend_from_slice(&[0x01u8; 20]);
        compact_nodes.extend_from_slice(&[192, 168, 1, 1]);
        compact_nodes.extend_from_slice(&[0x1F, 0x90]); // 8080
        // Node 2: id=0x02.., IP 10.0.0.2:6881
        compact_nodes.extend_from_slice(&[0x02u8; 20]);
        compact_nodes.extend_from_slice(&[10, 0, 0, 2]);
        compact_nodes.extend_from_slice(&[0x1A, 0xE1]); // 6881

        let msg = DhtMessageBuilder::find_node_response(&tx, &self_id, &compact_nodes);

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_response());
        assert_eq!(decoded.t, tx.to_vec());

        let r = decoded.r.as_ref().expect("response must have r field");
        let id_bytes = r
            .dict_get(b"id")
            .and_then(|v| v.as_bytes())
            .expect("missing r.id");
        assert_eq!(id_bytes, &self_id[..]);

        let nodes_bytes = r
            .dict_get(b"nodes")
            .and_then(|v| v.as_bytes())
            .expect("missing r.nodes");
        assert_eq!(nodes_bytes, &compact_nodes[..]);

        // Cross-check with the existing extractor from client.rs
        let extracted =
            crate::bittorrent::dht::client::extract_compact_nodes_from_response(&decoded);
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0].0.port(), 8080);
        assert_eq!(extracted[1].0.port(), 6881);
        assert_eq!(extracted[0].1, [0x01u8; 20]);
        assert_eq!(extracted[1].1, [0x02u8; 20]);
    }

    #[test]
    fn test_get_peers_response_with_peers_encode_decode() {
        let tx = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let self_id = [0xCCu8; 20];
        let token = b"tok123";

        let peers: Vec<std::net::SocketAddr> = vec![
            "192.168.1.1:8080".parse().unwrap(),
            "10.0.0.2:6881".parse().unwrap(),
        ];

        let msg = DhtMessageBuilder::get_peers_response_with_peers(&tx, &self_id, token, &peers);

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_response());
        assert_eq!(decoded.t, tx.to_vec());

        let r = decoded.r.as_ref().expect("response must have r field");
        let id_bytes = r
            .dict_get(b"id")
            .and_then(|v| v.as_bytes())
            .expect("missing r.id");
        assert_eq!(id_bytes, &self_id[..]);

        let token_bytes = r
            .dict_get(b"token")
            .and_then(|v| v.as_bytes())
            .expect("missing r.token");
        assert_eq!(token_bytes, &token[..]);

        let values = r
            .dict_get(b"values")
            .and_then(|v| v.as_list())
            .expect("missing r.values");
        assert_eq!(values.len(), 2);

        // Cross-check with the existing extractor from client.rs
        let extracted =
            crate::bittorrent::dht::client::extract_compact_peers_from_response(&decoded);
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0], peers[0]);
        assert_eq!(extracted[1], peers[1]);
    }

    #[test]
    fn test_get_peers_response_with_peers_empty() {
        let tx = [0x01u8];
        let self_id = [0x00u8; 20];
        let token = b"";
        let peers: Vec<std::net::SocketAddr> = vec![];

        let msg = DhtMessageBuilder::get_peers_response_with_peers(&tx, &self_id, token, &peers);

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        let r = decoded.r.as_ref().expect("response must have r field");
        let values = r
            .dict_get(b"values")
            .and_then(|v| v.as_list())
            .expect("missing r.values");
        assert!(values.is_empty());

        let extracted =
            crate::bittorrent::dht::client::extract_compact_peers_from_response(&decoded);
        assert!(extracted.is_empty());
    }

    #[test]
    fn test_get_peers_response_with_nodes_encode_decode() {
        let tx = [0x55u8, 0x66];
        let self_id = [0xDDu8; 20];
        let token = b"node-token";

        // Build 1 compact node (26 bytes)
        let mut compact_nodes = Vec::new();
        compact_nodes.extend_from_slice(&[0x99u8; 20]);
        compact_nodes.extend_from_slice(&[172, 16, 0, 1]);
        compact_nodes.extend_from_slice(&[0x1F, 0x90]); // 8080

        let msg =
            DhtMessageBuilder::get_peers_response_with_nodes(&tx, &self_id, token, &compact_nodes);

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_response());
        assert_eq!(decoded.t, tx.to_vec());

        let r = decoded.r.as_ref().expect("response must have r field");
        let id_bytes = r
            .dict_get(b"id")
            .and_then(|v| v.as_bytes())
            .expect("missing r.id");
        assert_eq!(id_bytes, &self_id[..]);

        let token_bytes = r
            .dict_get(b"token")
            .and_then(|v| v.as_bytes())
            .expect("missing r.token");
        assert_eq!(token_bytes, &token[..]);

        let nodes_bytes = r
            .dict_get(b"nodes")
            .and_then(|v| v.as_bytes())
            .expect("missing r.nodes");
        assert_eq!(nodes_bytes, &compact_nodes[..]);

        // Cross-check with the existing extractor from client.rs
        let extracted =
            crate::bittorrent::dht::client::extract_compact_nodes_from_response(&decoded);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].0.port(), 8080);
        assert_eq!(extracted[0].1, [0x99u8; 20]);
    }

    #[test]
    fn test_announce_peer_response_encode_decode() {
        let tx = [0x77u8, 0x88, 0x99];
        let self_id = [0xEEu8; 20];
        let msg = DhtMessageBuilder::announce_peer_response(&tx, &self_id);

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_response());
        assert_eq!(decoded.t, tx.to_vec());

        let r = decoded.r.as_ref().expect("response must have r field");
        let id_bytes = r
            .dict_get(b"id")
            .and_then(|v| v.as_bytes())
            .expect("missing r.id");
        assert_eq!(id_bytes, &self_id[..]);

        // announce_peer response should only contain "id" — no nodes/values/token
        assert!(r.dict_get(b"nodes").is_none());
        assert!(r.dict_get(b"values").is_none());
        assert!(r.dict_get(b"token").is_none());
    }

    #[test]
    fn test_error_response_encode_decode() {
        let tx = [0xABu8, 0xCD];
        let msg = DhtMessageBuilder::error_response(&tx, 203, "Invalid token");

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_error());
        assert_eq!(decoded.t, tx.to_vec());
        assert_eq!(decoded.e, Some((203, "Invalid token".to_string())));
    }

    #[test]
    fn test_error_response_generic_error() {
        let tx = [0x00u8];
        let msg = DhtMessageBuilder::error_response(&tx, 202, "Server Error");

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert!(decoded.is_error());
        assert_eq!(decoded.t, tx.to_vec());
        assert_eq!(decoded.e, Some((202, "Server Error".to_string())));
    }

    #[test]
    fn test_ping_response_echoes_arbitrary_tx_length() {
        // BEP 0005 allows variable-length transaction IDs (typically 2 bytes).
        // Verify a 2-byte tx is echoed back correctly.
        let tx = [0x0Au8, 0x0B];
        let self_id = [0x12u8; 20];
        let msg = DhtMessageBuilder::ping_response(&tx, &self_id);

        let encoded = msg.encode().unwrap();
        let decoded = DhtMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.t, tx.to_vec());
    }
}
