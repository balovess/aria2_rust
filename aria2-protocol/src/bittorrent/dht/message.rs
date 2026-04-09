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
}
