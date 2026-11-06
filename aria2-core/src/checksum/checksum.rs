use std::path::Path;

use super::message_digest::{HashType, MessageDigest};
use crate::error::{Aria2Error, Result};

#[derive(Debug, Clone)]
pub struct Checksum {
    hash_type: HashType,
    expected_hex: String,
}

impl Checksum {
    pub fn new(hash_type: HashType, hex_digest: &str) -> Result<Self> {
        let hex = hex_digest.trim().to_lowercase();
        if hex.is_empty() {
            return Err(Aria2Error::Parse(
                "checksum value cannot be empty".to_string(),
            ));
        }
        let expected_len = hash_type.digest_length() * 2;
        if hex.len() != expected_len {
            return Err(Aria2Error::Parse(format!(
                "{} checksum length mismatch: expected {} hex chars, got {}",
                hash_type.as_str(),
                expected_len,
                hex.len()
            )));
        }
        for (i, ch) in hex.chars().enumerate() {
            if !ch.is_ascii_hexdigit() {
                return Err(Aria2Error::Parse(format!(
                    "invalid hex character '{}' at position {}",
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
            .ok_or_else(|| Aria2Error::Parse(format!("unknown hash algorithm: {}", type_str)))?;
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
        computed.eq_ignore_ascii_case(&self.expected_hex)
    }

    /// Verify an owned in-memory payload without hashing on the async worker.
    ///
    /// The payload is returned from the blocking worker so callers can retain
    /// it for the completed in-memory download without cloning the buffer.
    pub async fn verify_async(&self, data: Vec<u8>) -> Result<(Vec<u8>, bool)> {
        let hash_type = self.hash_type;
        let expected_hex = self.expected_hex.clone();
        crate::checksum::hash_worker_pool::shared()
            .run(
                move || {
                    Ok({
                        let computed = MessageDigest::hash_hex(hash_type, &data);
                        let verified = computed.eq_ignore_ascii_case(&expected_hex);
                        (data, verified)
                    })
                },
                "checksum",
            )
            .await
    }

    pub fn create_validator<'a>(&'a self) -> ChecksumValidator<'a> {
        ChecksumValidator {
            checksum: self,
            digest: MessageDigest::new(self.hash_type),
        }
    }
}

/// Verify a file incrementally without loading it into memory.
pub async fn verify_file(path: &Path, checksum: &Checksum) -> Result<bool> {
    let open_path = path.to_path_buf();
    let mut file = crate::filesystem::disk_io_pool::shared()
        .run(
            move || {
                std::fs::File::open(&open_path).map_err(|error| {
                    Aria2Error::Io(format!("Failed to open {}: {error}", open_path.display()))
                })
            },
            "checksum file open",
        )
        .await?;
    let mut digest = MessageDigest::new(checksum.hash_type);

    loop {
        let read_path = path.to_path_buf();
        let (returned_file, read_result) = crate::filesystem::disk_io_pool::shared()
            .run(
                move || {
                    use std::io::Read;
                    let mut buffer = vec![0u8; 65536];
                    let result = file
                        .read(&mut buffer)
                        .map(|bytes_read| (buffer, bytes_read))
                        .map_err(|error| {
                            Aria2Error::Io(format!(
                                "Failed to read {}: {error}",
                                read_path.display()
                            ))
                        });
                    Ok((file, result))
                },
                "checksum file read",
            )
            .await?;
        file = returned_file;
        let (mut buffer, bytes_read) = read_result?;
        if bytes_read == 0 {
            break;
        }
        buffer.truncate(bytes_read);
        let (next_digest, returned_buffer) = update_digest_async(digest, buffer).await?;
        digest = next_digest;
        buffer = returned_buffer;
        buffer.resize(65536, 0);
    }

    let computed = finalize_digest_async(digest).await?;
    Ok(computed.eq_ignore_ascii_case(&checksum.expected_hex))
}

async fn update_digest_async(
    digest: MessageDigest,
    data: Vec<u8>,
) -> Result<(MessageDigest, Vec<u8>)> {
    crate::checksum::hash_worker_pool::shared()
        .run(
            move || {
                Ok({
                    let mut digest = digest;
                    digest.update(&data);
                    (digest, data)
                })
            },
            "checksum digest update",
        )
        .await
}

async fn finalize_digest_async(digest: MessageDigest) -> Result<String> {
    crate::checksum::hash_worker_pool::shared()
        .run(|| Ok(digest.finalize_hex()), "checksum digest finalize")
        .await
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
        Ok(computed.eq_ignore_ascii_case(&self.checksum.expected_hex))
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

        let cs = Checksum::from_type_and_value(
            "SHA-256",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
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
        let cs = Checksum::new(
            HashType::Sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();

        let mut validator = cs.create_validator();
        validator.update(b"a");
        validator.update(b"bc");
        assert!(validator.finalize().unwrap());

        assert!(
            cs.verify(b"abc"),
            "streaming verification should match one-shot verification"
        );
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

    #[tokio::test]
    async fn test_verify_file_streams_and_accepts_matching_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        tokio::fs::write(&path, b"aria2-rust").await.unwrap();
        let checksum = Checksum::new(
            HashType::Sha256,
            "b467a8c596e15709e4805cb631a2d7cc8a2cf287869eb2f994cb8100d8ae809c",
        )
        .unwrap();

        assert!(verify_file(&path, &checksum).await.unwrap());
    }

    #[tokio::test]
    async fn test_verify_file_rejects_mismatch_and_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        tokio::fs::write(&path, b"payload").await.unwrap();
        let checksum = Checksum::new(
            HashType::Sha256,
            "239f59ed55e737c77147cf55ad0c1b030f1f5f7f5e5b6e7c7b5d8a1a3f2a4a6b",
        )
        .unwrap();

        assert!(!verify_file(&path, &checksum).await.unwrap());
        assert!(
            verify_file(&dir.path().join("missing.bin"), &checksum)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_verify_async_returns_owned_payload_without_copying_contract() {
        let checksum = Checksum::new(
            HashType::Sha256,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        )
        .unwrap();
        let payload = b"hello world".to_vec();

        let (returned, verified) = checksum.verify_async(payload).await.unwrap();

        assert!(verified);
        assert_eq!(returned, b"hello world");
    }
}
