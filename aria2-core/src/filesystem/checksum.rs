use std::io::{Read, Cursor};
use crate::error::{Aria2Error, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChecksumType {
    Md5,
    Sha1,
    Sha256,
    Adler32,
}

pub struct Checksum {
    checksum_type: ChecksumType,
    expected: String,
}

impl Checksum {
    pub fn new(checksum_type: ChecksumType, expected: String) -> Self {
        Checksum {
            checksum_type,
            expected,
        }
    }

    pub fn checksum_type(&self) -> ChecksumType {
        self.checksum_type
    }

    pub fn expected(&self) -> &str {
        &self.expected
    }

    pub fn verify<R: Read>(&self, mut reader: R) -> Result<bool> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        
        let actual = match self.checksum_type {
            ChecksumType::Md5 => md5_hex(&data),
            ChecksumType::Sha1 => sha1_hex(&data),
            ChecksumType::Sha256 => sha256_hex(&data),
            ChecksumType::Adler32 => adler32_hex(&data),
        };

        let matches = actual.to_lowercase() == self.expected.to_lowercase();
        
        if matches {
            Ok(true)
        } else {
            Err(Aria2Error::Checksum(format!(
                "校验失败: 期望={}, 实际={}",
                self.expected, actual
            )))
        }
    }

    pub fn verify_bytes(&self, data: &[u8]) -> Result<bool> {
        let actual = match self.checksum_type {
            ChecksumType::Md5 => md5_hex(data),
            ChecksumType::Sha1 => sha1_hex(data),
            ChecksumType::Sha256 => sha256_hex(data),
            ChecksumType::Adler32 => adler32_hex(data),
        };

        let matches = actual.to_lowercase() == self.expected.to_lowercase();
        
        if matches {
            Ok(true)
        } else {
            Err(Aria2Error::Checksum(format!(
                "校验失败: 期望={}, 实际={}",
                self.expected, actual
            )))
        }
    }
}

fn md5_hex(data: &[u8]) -> String {
    let result = md5::compute(data);
    format!("{:x}", result)
}

fn sha1_hex(data: &[u8]) -> String {
    use digest::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(data);
    let result = hasher.finalize();
    format!("{:x}", result)
}

fn sha256_hex(data: &[u8]) -> String {
    use digest::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    format!("{:x}", result)
}

fn adler32_hex(data: &[u8]) -> String {
    let hash = adler32::adler32(Cursor::new(data)).unwrap_or(0);
    format!("{:08x}", hash)
}
