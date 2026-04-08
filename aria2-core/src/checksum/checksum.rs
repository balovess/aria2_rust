use crate::error::{Aria2Error, Result};
use super::message_digest::{HashType, MessageDigest};

#[derive(Debug, Clone)]
pub struct Checksum {
    hash_type: HashType,
    expected_hex: String,
}

impl Checksum {
    pub fn new(hash_type: HashType, hex_digest: &str) -> Result<Self> {
        let hex = hex_digest.trim().to_lowercase();
        if hex.is_empty() {
            return Err(Aria2Error::Parse("校验和值不能为空".to_string()));
        }
        let expected_len = hash_type.digest_length() * 2;
        if hex.len() != expected_len {
            return Err(Aria2Error::Parse(format!(
                "{} 校验和长度不匹配: 期望 {} 字符(hex), 实际 {}",
                hash_type.as_str(),
                expected_len,
                hex.len()
            )));
        }
        for (i, ch) in hex.chars().enumerate() {
            if !ch.is_ascii_hexdigit() {
                return Err(Aria2Error::Parse(format!(
                    "校验和包含非法字符 '{}' 在位置 {}",
                    ch, i
                )));
            }
        }

        Ok(Checksum {
            hash_type,
            expected_hex: hex,
        })
    }

    pub fn from_type_and_value(type_str: &str, value_str: &str) -> Result<Self> {
        let ht = HashType::from_str(type_str)
            .ok_or_else(|| Aria2Error::Parse(format!("未知哈希算法: {}", type_str)))?;
        Self::new(ht, value_str)
    }

    pub fn hash_type(&self) -> HashType {
        self.hash_type
    }

    pub fn expected_hex(&self) -> &str {
        &self.expected_hex
    }

    pub fn is_empty(&self) -> bool {
        self.expected_hex.is_empty()
    }

    pub fn verify(&self, data: &[u8]) -> bool {
        let computed = MessageDigest::hash_hex(self.hash_type, data);
        computed == self.expected_hex
    }

    pub fn create_validator<'a>(&'a self) -> ChecksumValidator<'a> {
        ChecksumValidator {
            checksum: self,
            digest: MessageDigest::new(self.hash_type),
        }
    }
}

pub struct ChecksumValidator<'a> {
    checksum: &'a Checksum,
    digest: MessageDigest,
}

impl<'a> ChecksumValidator<'a> {
    pub fn update(&mut self, data: &[u8]) {
        self.digest.update(data);
    }

    pub fn finalize(self) -> Result<bool> {
        let computed = self.digest.finalize_hex();
        Ok(computed == self.checksum.expected_hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checksum_verify_correct_data_md5() {
        let cs = Checksum::new(HashType::Md5, "d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert!(cs.verify(b""));
    }

    #[test]
    fn test_checksum_verify_wrong_data_rejected() {
        let cs = Checksum::new(HashType::Md5, "d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert!(!cs.verify(b"not empty"));
    }

    #[test]
    fn test_checksum_verify_sha1() {
        let cs = Checksum::new(HashType::Sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709").unwrap();
        assert!(cs.verify(b""));
        assert!(!cs.verify(b"x"));
    }

    #[test]
    fn test_checksum_from_type_string() {
        let cs = Checksum::from_type_and_value("md5", "d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert_eq!(cs.hash_type(), HashType::Md5);

        let cs = Checksum::from_type_and_value("SHA-256", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").unwrap();
        assert_eq!(cs.hash_type(), HashType::Sha256);
    }

    #[test]
    fn test_checksum_invalid_hex_rejected() {
        assert!(Checksum::new(HashType::Md5, "zzz").is_err());
        assert!(Checksum::new(HashType::Md5, "").is_err());
        assert!(Checksum::new(HashType::Md5, "abc").is_err()); // MD5 needs 32 hex chars
    }

    #[test]
    fn test_checksum_unknown_algorithm() {
        assert!(Checksum::from_type_and_value("blake3", "abc").is_err());
    }

    #[test]
    fn test_validator_streaming_matches_one_shot() {
        let cs = Checksum::new(HashType::Sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap();

        let mut validator = cs.create_validator();
        validator.update(b"a");
        validator.update(b"bc");
        assert!(validator.finalize().unwrap());

        assert!(cs.verify(b"abc"), "流式验证应与一次性验证一致");
    }

    #[test]
    fn test_checksum_case_insensitive() {
        let upper = Checksum::new(HashType::Md5, "D41D8CD98F00B204E9800998ECF8427E").unwrap();
        let lower = Checksum::new(HashType::Md5, "d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert_eq!(upper.expected_hex(), lower.expected_hex());
    }

    #[test]
    fn test_checksum_is_empty_false_for_valid() {
        let cs = Checksum::new(HashType::Md5, "d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert!(!cs.is_empty());
    }
}
