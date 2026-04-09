use sha1::{Digest, Sha1};

use crate::bittorrent::bencode::codec::BencodeValue;

#[derive(Debug, Clone)]
pub struct InfoHash {
    pub bytes: [u8; 20],
}

impl InfoHash {
    pub fn from_info_value(info: &BencodeValue) -> Self {
        let info_bytes = info.encode();
        let hash = Sha1::digest(&info_bytes);
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&hash);
        Self { bytes }
    }

    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Self { bytes }
    }

    pub fn as_hex(&self) -> String {
        self.bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn as_upper_hex(&self) -> String {
        self.bytes.iter().map(|b| format!("{:02X}", b)).collect()
    }
}

impl PartialEq for InfoHash {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for InfoHash {}

impl std::hash::Hash for InfoHash {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_info_hash_from_simple_dict() {
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(100));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(16384));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));
        let info_val = BencodeValue::Dict(info);

        let ih = InfoHash::from_info_value(&info_val);
        assert_eq!(ih.as_hex().len(), 40);
    }

    #[test]
    fn test_info_hash_deterministic() {
        let mut info = BTreeMap::new();
        info.insert(b"length".to_vec(), BencodeValue::Int(12345));
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"file.txt".to_vec()));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(524288));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));
        let val = BencodeValue::Dict(info.clone());

        let h1 = InfoHash::from_info_value(&val);
        let h2 = InfoHash::from_info_value(&val);
        assert_eq!(h1.as_hex(), h2.as_hex());
    }

    #[test]
    fn test_info_hash_hex_formats() {
        let mut bytes = [0u8; 20];
        bytes[0] = 0xAB;
        bytes[1] = 0xCD;
        bytes[2] = 0xEF;
        let ih = InfoHash::from_bytes(bytes);
        assert!(ih.as_hex().starts_with("abcdef"));
        assert!(ih.as_upper_hex().starts_with("ABCDEF"));
    }
}
