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
    pub const GET: &'static str = "get";
    pub const PUT: &'static str = "put";
    pub const SAMPLE_INFOHASHES: &'static str = "sample_infohashes";
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

    pub fn encode(&self) -> Vec<u8> {
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

        BencodeValue::Dict(dict).encode()
    }

    pub fn decode(data: &[u8]) -> Result<Self, String> {
        let (root, _) = BencodeValue::decode(data)?;

        let t = root
            .dict_get(b"t")
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec())
            .ok_or("Missing 't' field")?;

        let y_bytes = root
            .dict_get(b"y")
            .and_then(|v| v.as_bytes())
            .ok_or("Missing 'y' field")?;

        let y = match y_bytes {
            b"q" => DhtMessageType::Query,
            b"r" => DhtMessageType::Response,
            b"e" => DhtMessageType::Error,
            _ => return Err(format!("Invalid 'y' value: {:?}", y_bytes)),
        };

        match y {
            DhtMessageType::Query => {
                let q_str = root.dict_get_str("q").ok_or("Missing 'q' field")?;
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
                let result = root.dict_get(b"r").ok_or("Missing 'r' field")?;
                result.as_dict().ok_or("Invalid 'r' field")?;
                let node_id = result
                    .dict_get(b"id")
                    .and_then(BencodeValue::as_bytes)
                    .ok_or("Missing 'r.id' field")?;
                if node_id.len() != 20 {
                    return Err("Invalid 'r.id' field length".to_string());
                }
                Ok(Self {
                    t,
                    y,
                    q: None,
                    a: None,
                    r: Some(result.clone()),
                    e: None,
                })
            }
            DhtMessageType::Error => {
                let err_val = root
                    .dict_get(b"e")
                    .and_then(|v| v.as_list())
                    .ok_or("Missing 'e' field")?;
                if err_val.len() != 2 {
                    return Err("Invalid error format".to_string());
                }
                let code = err_val[0].as_int().ok_or("Invalid error code")?;
                let msg = err_val[1].as_bytes().ok_or("Invalid error message")?;
                Ok(Self {
                    t,
                    y,
                    q: None,
                    a: None,
                    r: None,
                    e: Some((code, String::from_utf8_lossy(msg).into_owned())),
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
/// [`crate::bittorrent::dht::compact::extract_compact_peers_from_response`].
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

    /// Build an `announce_peer` query using the opaque token returned by
    /// `get_peers`.
    pub fn announce_peer_with_token(
        transaction_id: u32,
        sender_id: &[u8; 20],
        info_hash: &[u8; 20],
        port: u16,
        token: &[u8],
    ) -> DhtMessage {
        let mut args_dict = std::collections::BTreeMap::new();
        args_dict.insert(b"id".to_vec(), BencodeValue::Bytes(sender_id.to_vec()));
        args_dict.insert(
            b"info_hash".to_vec(),
            BencodeValue::Bytes(info_hash.to_vec()),
        );
        args_dict.insert(b"port".to_vec(), BencodeValue::Int(port as i64));
        args_dict.insert(b"token".to_vec(), BencodeValue::Bytes(token.to_vec()));
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
        Self::find_node_response_with_nodes_field(tx, self_id, b"nodes", compact_nodes)
    }

    /// Build a find_node response carrying IPv6 compact nodes under `nodes6`.
    pub fn find_node_response6(tx: &[u8], self_id: &[u8; 20], compact_nodes: &[u8]) -> DhtMessage {
        Self::find_node_response_with_nodes_field(tx, self_id, b"nodes6", compact_nodes)
    }

    fn find_node_response_with_nodes_field(
        tx: &[u8],
        self_id: &[u8; 20],
        nodes_field: &[u8],
        compact_nodes: &[u8],
    ) -> DhtMessage {
        let mut r_dict = std::collections::BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(self_id.to_vec()));
        r_dict.insert(
            nodes_field.to_vec(),
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
        Self::get_peers_response_with_nodes_field(tx, self_id, token, b"nodes", compact_nodes)
    }

    /// Build a get_peers response carrying closest IPv6 nodes under `nodes6`.
    pub fn get_peers_response_with_nodes6(
        tx: &[u8],
        self_id: &[u8; 20],
        token: &[u8],
        compact_nodes: &[u8],
    ) -> DhtMessage {
        Self::get_peers_response_with_nodes_field(tx, self_id, token, b"nodes6", compact_nodes)
    }

    fn get_peers_response_with_nodes_field(
        tx: &[u8],
        self_id: &[u8; 20],
        token: &[u8],
        nodes_field: &[u8],
        compact_nodes: &[u8],
    ) -> DhtMessage {
        let mut r_dict = std::collections::BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(self_id.to_vec()));
        r_dict.insert(b"token".to_vec(), BencodeValue::Bytes(token.to_vec()));
        r_dict.insert(
            nodes_field.to_vec(),
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
mod tests;
