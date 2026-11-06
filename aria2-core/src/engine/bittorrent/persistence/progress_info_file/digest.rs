//! SHA-1 digest computation for progress file dedup.
//!
//! Replaces the previous XOR-based placeholder digest with a real SHA-1
//! hash, matching C++ aria2's `SHA1IOFile` dedup mechanism. This prevents
//! redundant writes when the serialized content has not changed.

use sha1::{Digest, Sha1};

/// Compute a SHA-1 digest of the serialized progress data.
///
/// Used for dedup: if the digest matches the last written file, the write
/// is skipped to avoid unnecessary disk I/O (especially important for
/// preventing wake-ups of sleeping disks).
///
/// Matches C++ `SHA1IOFile::digest()` behavior.
pub fn compute_sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut digest = [0u8; 20];
    digest.copy_from_slice(&result);
    digest
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha1_digest_deterministic() {
        let data = b"hello world";
        let d1 = compute_sha1_digest(data);
        let d2 = compute_sha1_digest(data);
        assert_eq!(d1, d2, "SHA-1 digest must be deterministic");
    }

    #[test]
    fn test_sha1_digest_different_inputs() {
        let d1 = compute_sha1_digest(b"foo");
        let d2 = compute_sha1_digest(b"bar");
        assert_ne!(d1, d2, "Different inputs must produce different digests");
    }

    #[test]
    fn test_sha1_digest_known_value() {
        // SHA-1 of empty string is well-known: da39a3ee5e6b4b0d3255bfef95601890afd80709
        let digest = compute_sha1_digest(b"");
        let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(hex, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn test_sha1_digest_length() {
        let digest = compute_sha1_digest(b"test data");
        assert_eq!(digest.len(), 20, "SHA-1 digest must be 20 bytes");
    }
}
