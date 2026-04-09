use crate::bittorrent::bencode::codec::BencodeValue;

pub const EXT_MESSAGE_ID: u8 = 20;
pub const UT_METADATA_ID: &str = "ut_metadata";
pub const METADATA_ID: &str = "metadata_size";

fn find_dict_entry<'a>(
    dict: &'a std::collections::BTreeMap<Vec<u8>, BencodeValue>,
    key: &[u8],
) -> Option<&'a BencodeValue> {
    dict.iter()
        .find(|(k, _)| k.as_slice() == key)
        .map(|(_, v)| v)
}

#[derive(Debug, Clone)]
pub enum UtMetadataMsg {
    Request(u32),
    Data(u32, Vec<u8>),
    Reject(u32),
}

pub struct ExtensionHandshake {
    pub m: std::collections::BTreeMap<String, i64>,
    pub metadata_size: Option<u64>,
    pub v: Option<String>,
}

impl ExtensionHandshake {
    pub fn new(metadata_size: u64) -> Self {
        let mut m = std::collections::BTreeMap::new();
        m.insert(UT_METADATA_ID.to_string(), 1);
        Self {
            m,
            metadata_size: Some(metadata_size),
            v: Some("aria2-rust/0.1.0".to_string()),
        }
    }

    pub fn to_bencode(&self) -> BencodeValue {
        let mut dict = std::collections::BTreeMap::new();

        let m_dict: BencodeValue = BencodeValue::Dict(
            self.m
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), BencodeValue::Int(*v)))
                .collect(),
        );
        dict.insert(b"m".to_vec(), m_dict);

        if let Some(size) = self.metadata_size {
            dict.insert(
                METADATA_ID.as_bytes().to_vec(),
                BencodeValue::Int(size as i64),
            );
        }
        if let Some(ref version) = self.v {
            dict.insert(
                b"v".to_vec(),
                BencodeValue::Bytes(version.as_bytes().to_vec()),
            );
        }

        BencodeValue::Dict(dict)
    }

    pub fn parse(data: &BencodeValue) -> Option<Self> {
        let dict = data.as_dict()?;

        let m_val = find_dict_entry(dict, b"m")?.as_dict()?;
        let mut m = std::collections::BTreeMap::new();
        for (k, v) in m_val {
            if let Ok(s) = std::str::from_utf8(k) {
                if let Some(id) = v.as_int() {
                    m.insert(s.to_string(), id);
                }
            }
        }

        let metadata_size = find_dict_entry(dict, METADATA_ID.as_bytes())
            .and_then(|v| v.as_int())
            .map(|i| i as u64);

        let v = find_dict_entry(dict, b"v")
            .and_then(|val| val.as_bytes())
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(|s| s.to_string());

        Some(Self {
            m,
            metadata_size,
            v,
        })
    }

    pub fn get_ut_metadata_id(&self) -> Option<i64> {
        self.m.get(UT_METADATA_ID).copied()
    }
}

impl UtMetadataMsg {
    pub fn encode(&self, ext_id: u8) -> Vec<u8> {
        use std::collections::BTreeMap;
        let mut dict = BTreeMap::new();
        match self {
            UtMetadataMsg::Request(piece) => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(0));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
            }
            UtMetadataMsg::Data(piece, data) => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(1));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
                dict.insert(b"data".to_vec(), BencodeValue::Bytes(data.clone()));
            }
            UtMetadataMsg::Reject(piece) => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(2));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
            }
        }

        let payload = BencodeValue::Dict(dict).encode();
        let len = (1 + payload.len()) as u32;
        let mut result = Vec::with_capacity(4 + 1 + payload.len());
        result.extend_from_slice(&len.to_be_bytes());
        result.push(ext_id);
        result.extend_from_slice(&payload);
        result
    }

    pub fn decode(payload: &[u8]) -> Result<Self, String> {
        let (val, _) = BencodeValue::decode(payload)
            .map_err(|e| format!("Failed to decode ut_metadata: {}", e))?;

        let dict = val.as_dict().ok_or("ut_metadata payload is not a dict")?;

        let msg_type = find_dict_entry(dict, b"msg_type")
            .and_then(|v| v.as_int())
            .ok_or("Missing msg_type")? as u32;

        let piece = find_dict_entry(dict, b"piece")
            .and_then(|v| v.as_int())
            .ok_or("Missing piece")? as u32;

        match msg_type {
            0 => Ok(UtMetadataMsg::Request(piece)),
            1 => {
                let data = find_dict_entry(dict, b"data")
                    .and_then(|v| v.as_bytes())
                    .ok_or("Missing data in ut_metadata Data msg")?
                    .to_vec();
                Ok(UtMetadataMsg::Data(piece, data))
            }
            2 => Ok(UtMetadataMsg::Reject(piece)),
            _ => Err(format!("Unknown msg_type: {}", msg_type)),
        }
    }
}

pub struct MetadataCollector {
    total_size: u64,
    collected: Vec<Option<Vec<u8>>>,
    #[allow(dead_code)]
    piece_size: u32,
}

impl MetadataCollector {
    pub fn new(total_size: u64, piece_size: u32) -> Self {
        let num_pieces = ((total_size + piece_size as u64 - 1) / piece_size as u64) as usize;
        Self {
            total_size,
            collected: vec![None; num_pieces],
            piece_size,
        }
    }

    pub fn add_piece(&mut self, piece_idx: u32, data: &[u8]) -> bool {
        let idx = piece_idx as usize;
        if idx >= self.collected.len() {
            return false;
        }
        if self.collected[idx].is_some() {
            return false;
        }
        self.collected[idx] = Some(data.to_vec());
        true
    }

    pub fn is_complete(&self) -> bool {
        self.collected.iter().all(|p| p.is_some())
    }

    pub fn assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut result = Vec::with_capacity(self.total_size as usize);
        for piece in &self.collected {
            result.extend(piece.as_ref().unwrap());
        }
        Some(result)
    }

    pub fn progress(&self) -> f64 {
        let done = self.collected.iter().filter(|p| p.is_some()).count();
        done as f64 / self.collected.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_handshake_roundtrip() {
        let hs = ExtensionHandshake::new(12345);
        let encoded = hs.to_bencode();
        let parsed = ExtensionHandshake::parse(&encoded).unwrap();
        assert_eq!(parsed.metadata_size, Some(12345));
        assert_eq!(parsed.get_ut_metadata_id(), Some(1));
    }

    #[test]
    fn test_ut_metadata_request_encode_decode() {
        let msg = UtMetadataMsg::Request(0);
        let encoded = msg.encode(1);
        let decoded = UtMetadataMsg::decode(&encoded[5..]).unwrap();
        match decoded {
            UtMetadataMsg::Request(p) => assert_eq!(p, 0),
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_ut_metadata_data_encode_decode() {
        let data = b"fake torrent metadata".to_vec();
        let msg = UtMetadataMsg::Data(0, data.clone());
        let encoded = msg.encode(2);
        let decoded = UtMetadataMsg::decode(&encoded[5..]).unwrap();
        match decoded {
            UtMetadataMsg::Data(p, d) => {
                assert_eq!(p, 0);
                assert_eq!(&d[..], &data[..]);
            }
            _ => panic!("Expected Data"),
        }
    }

    #[test]
    fn test_metadata_collector() {
        let mut collector = MetadataCollector::new(2000, 1000);
        assert_eq!(collector.collected.len(), 2);
        assert!(!collector.is_complete());

        collector.add_piece(0, &vec![0xAB; 1000]);
        assert!(!collector.is_complete());
        assert!((collector.progress() - 0.5).abs() < 0.01);

        collector.add_piece(1, &vec![0xCD; 1000]);
        assert!(collector.is_complete());

        let assembled = collector.assemble().unwrap();
        assert_eq!(assembled.len(), 2000);
    }
}
