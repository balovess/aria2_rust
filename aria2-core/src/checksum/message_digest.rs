use digest::Digest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashType {
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Adler32,
}

impl HashType {
    pub fn from_str(s: &str) -> Option<HashType> {
        match s.to_lowercase().as_str() {
            "md5" => Some(HashType::Md5),
            "sha-1" | "sha1" => Some(HashType::Sha1),
            "sha-256" | "sha256" => Some(HashType::Sha256),
            "sha-512" | "sha512" => Some(HashType::Sha512),
            "adler32" => Some(HashType::Adler32),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HashType::Md5 => "md5",
            HashType::Sha1 => "sha1",
            HashType::Sha256 => "sha256",
            HashType::Sha512 => "sha512",
            HashType::Adler32 => "adler32",
        }
    }

    pub fn digest_length(&self) -> usize {
        match self {
            HashType::Md5 => 16,
            HashType::Sha1 => 20,
            HashType::Sha256 => 32,
            HashType::Sha512 => 64,
            HashType::Adler32 => 4,
        }
    }

    pub fn all_supported() -> Vec<HashType> {
        vec![
            HashType::Md5,
            HashType::Sha1,
            HashType::Sha256,
            HashType::Sha512,
            HashType::Adler32,
        ]
    }
}

enum DigestInner {
    Md5(md5::Context),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Adler32(Vec<u8>),
}

pub struct MessageDigest {
    inner: DigestInner,
}

impl MessageDigest {
    pub fn new(algo: HashType) -> Self {
        let inner = match algo {
            HashType::Md5 => DigestInner::Md5(md5::Context::new()),
            HashType::Sha1 => DigestInner::Sha1(sha1::Sha1::new()),
            HashType::Sha256 => DigestInner::Sha256(sha2::Sha256::new()),
            HashType::Sha512 => DigestInner::Sha512(sha2::Sha512::new()),
            HashType::Adler32 => DigestInner::Adler32(Vec::new()),
        };
        MessageDigest { inner }
    }

    pub fn update(&mut self, data: &[u8]) {
        match &mut self.inner {
            DigestInner::Md5(d) => d.consume(data),
            DigestInner::Sha1(d) => d.update(data),
            DigestInner::Sha256(d) => d.update(data),
            DigestInner::Sha512(d) => d.update(data),
            DigestInner::Adler32(buf) => buf.extend_from_slice(data),
        }
    }

    pub fn finalize(self) -> Vec<u8> {
        match self.inner {
            DigestInner::Md5(d) => d.compute().to_vec(),
            DigestInner::Sha1(d) => d.finalize().to_vec(),
            DigestInner::Sha256(d) => d.finalize().to_vec(),
            DigestInner::Sha512(d) => d.finalize().to_vec(),
            DigestInner::Adler32(buf) => {
                let checksum = adler32::adler32(&buf[..]).unwrap_or(1);
                checksum.to_le_bytes().to_vec()
            }
        }
    }

    pub fn finalize_hex(self) -> String {
        let bytes = self.finalize();
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn digest_length(&self) -> usize {
        match &self.inner {
            DigestInner::Md5(_) => 16,
            DigestInner::Sha1(_) => 20,
            DigestInner::Sha256(_) => 32,
            DigestInner::Sha512(_) => 64,
            DigestInner::Adler32(_) => 4,
        }
    }

    pub fn reset(&mut self) {
        match &mut self.inner {
            DigestInner::Md5(d) => *d = md5::Context::new(),
            DigestInner::Sha1(d) => *d = sha1::Sha1::new(),
            DigestInner::Sha256(d) => *d = sha2::Sha256::new(),
            DigestInner::Sha512(d) => *d = sha2::Sha512::new(),
            DigestInner::Adler32(s) => *s = Vec::new(),
        }
    }

    pub fn hash_data(algo: HashType, data: &[u8]) -> Vec<u8> {
        let mut digest = Self::new(algo);
        digest.update(data);
        digest.finalize()
    }

    pub fn hash_hex(algo: HashType, data: &[u8]) -> String {
        let mut digest = Self::new(algo);
        digest.update(data);
        digest.finalize_hex()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_type_from_str() {
        assert_eq!(HashType::from_str("md5"), Some(HashType::Md5));
        assert_eq!(HashType::from_str("MD5"), Some(HashType::Md5));
        assert_eq!(HashType::from_str("sha-1"), Some(HashType::Sha1));
        assert_eq!(HashType::from_str("SHA1"), Some(HashType::Sha1));
        assert_eq!(HashType::from_str("sha-256"), Some(HashType::Sha256));
        assert_eq!(HashType::from_str("sha256"), Some(HashType::Sha256));
        assert_eq!(HashType::from_str("sha-512"), Some(HashType::Sha512));
        assert_eq!(HashType::from_str("adler32"), Some(HashType::Adler32));
        assert_eq!(HashType::from_str("unknown"), None);
    }

    #[test]
    fn test_md5_known_vector() {
        let hex = MessageDigest::hash_hex(HashType::Md5, b"");
        assert_eq!(hex, "d41d8cd98f00b204e9800998ecf8427e");

        let hex = MessageDigest::hash_hex(HashType::Md5, b"hello world");
        assert_eq!(hex, "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn test_sha1_known_vector() {
        let hex = MessageDigest::hash_hex(HashType::Sha1, b"");
        assert_eq!(hex, "da39a3ee5e6b4b0d3255bfef95601890afd80709");

        let hex = MessageDigest::hash_hex(
            HashType::Sha1,
            b"The quick brown fox jumps over the lazy dog",
        );
        assert_eq!(hex, "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12");
    }

    #[test]
    fn test_sha256_known_vector() {
        let hex = MessageDigest::hash_hex(HashType::Sha256, b"");
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let hex = MessageDigest::hash_hex(HashType::Sha256, b"abc");
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_adler32_basic() {
        let bytes = MessageDigest::hash_data(HashType::Adler32, b"hello world");
        assert_eq!(bytes.len(), 4);

        let empty = MessageDigest::hash_data(HashType::Adler32, b"");
        assert_eq!(empty.len(), 4);
    }

    #[test]
    fn test_finalize_hex_format_lowercase() {
        let hex = MessageDigest::hash_hex(HashType::Md5, b"test");
        for ch in hex.chars() {
            assert!(
                ch.is_ascii_digit() || ('a'..='f').contains(&ch),
                "hex 应为小写: {}",
                hex
            );
        }
    }

    #[test]
    fn test_digest_length_matches() {
        for ht in HashType::all_supported() {
            let md = MessageDigest::new(ht);
            assert_eq!(md.digest_length(), ht.digest_length());
        }
    }

    #[test]
    fn test_streaming_vs_one_shot() {
        let one_shot = MessageDigest::hash_hex(HashType::Sha256, b"hello world");

        let mut streaming = MessageDigest::new(HashType::Sha256);
        streaming.update(b"hello ");
        streaming.update(b"world");
        let streaming_hex = streaming.finalize_hex();

        assert_eq!(one_shot, streaming_hex);
    }

    #[test]
    fn test_different_data_different_hash() {
        let h1 = MessageDigest::hash_hex(HashType::Md5, b"first data");
        let h2 = MessageDigest::hash_hex(HashType::Md5, b"second data");
        assert_ne!(h1, h2, "不同数据应产生不同哈希值");
    }
}
