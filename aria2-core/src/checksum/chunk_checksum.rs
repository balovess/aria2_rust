use super::message_digest::{HashType, MessageDigest};

#[derive(Debug, Clone)]
pub struct ChunkChecksum {
    hash_type: HashType,
    piece_hashes: Vec<String>,
    piece_length: u64,
}

impl ChunkChecksum {
    pub fn new(hash_type: HashType, piece_hashes: Vec<String>, piece_length: u64) -> Self {
        ChunkChecksum {
            hash_type,
            piece_hashes,
            piece_length,
        }
    }

    pub fn hash_type(&self) -> HashType {
        self.hash_type
    }

    pub fn piece_count(&self) -> usize {
        self.piece_hashes.len()
    }

    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    pub fn estimated_data_length(&self) -> u64 {
        if self.piece_count() == 0 {
            return 0;
        }
        (self.piece_count() as u64 - 1) * self.piece_length
            + (self.piece_length as usize).min(256)
            as u64
    }

    pub fn verify_chunk(&self, chunk_data: &[u8], index: usize) -> bool {
        if index >= self.piece_hashes.len() {
            return false;
        }
        let computed = MessageDigest::hash_hex(self.hash_type, chunk_data);
        computed == self.piece_hashes[index]
    }

    pub fn piece_hash(&self, index: usize) -> Option<&String> {
        self.piece_hashes.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_chunk_checksum() -> ChunkChecksum {
        let hashes = vec![
            MessageDigest::hash_hex(HashType::Sha1, &[0u8; 1024]),
            MessageDigest::hash_hex(HashType::Sha1, &[1u8; 1024]),
            MessageDigest::hash_hex(HashType::Sha1, &vec![2u8; 512]),
        ];
        ChunkChecksum::new(HashType::Sha1, hashes, 1024)
    }

    #[test]
    fn test_chunk_checksum_verify_correct_chunks() {
        let cc = make_test_chunk_checksum();
        assert!(cc.verify_chunk(&[0u8; 1024], 0));
        assert!(cc.verify_chunk(&[1u8; 1024], 1));
    }

    #[test]
    fn test_chunk_checksum_reject_wrong_data() {
        let cc = make_test_chunk_checksum();
        assert!(!cc.verify_chunk(&[99u8; 1024], 0));
    }

    #[test]
    fn test_chunk_checksum_out_of_bounds_returns_false() {
        let cc = make_test_chunk_checksum();
        assert!(!cc.verify_chunk(&[0u8; 100], 99));
    }

    #[test]
    fn test_chunk_checksum_piece_count() {
        let cc = make_test_chunk_checksum();
        assert_eq!(cc.piece_count(), 3);
    }

    #[test]
    fn test_chunk_checksum_piece_hash_access() {
        let cc = make_test_chunk_checksum();
        assert!(cc.piece_hash(0).is_some());
        assert!(cc.piece_hash(2).is_some());
        assert!(cc.piece_hash(3).is_none());
    }

    #[test]
    fn test_chunk_checksum_empty() {
        let cc = ChunkChecksum::new(HashType::Md5, vec![], 16384);
        assert_eq!(cc.piece_count(), 0);
        assert_eq!(cc.estimated_data_length(), 0);
    }

    #[test]
    fn test_chunk_checksum_last_piece_smaller_than_standard() {
        let cc = make_test_chunk_checksum();
        assert!(cc.verify_chunk(&vec![2u8; 512], 2));
        assert!(!cc.verify_chunk(&vec![2u8; 1024], 2));
    }
}
