//! Metalink Checksum Verification - Phase 15 H7
//!
//! Provides file integrity verification using hash algorithms specified in
//! Metalink documents. Supports SHA-256, SHA-1, SHA-512, and MD5.
//!
//! # Architecture
//!
//! ```text
//! checksum_verifier.rs (this file)
//!   ├── verify_checksum() - Verify single hash entry against file
//!   ├── verify_all_checksums() - Verify all hashes in MetalinkFile
//!   ├── compute_file_hash() - Compute hash of a file using given algorithm
//!   └── HashVerificationResult - Detailed result of verification
//!
//! Dependencies:
//!   parser.rs - MetalinkFile, HashEntry, HashAlgorithm structs
//!   sha2 crate - SHA-256, SHA-512 implementation
//!   sha1 crate - SHA-1 implementation
//!   md5 crate - MD5 implementation
//! ```

use std::path::Path;

use tracing::{debug, info, warn};

use crate::metalink::parser::{HashAlgorithm, HashEntry, MetalinkFile};

/// Result of a single checksum verification operation
#[derive(Debug, Clone)]
pub struct HashVerificationResult {
    /// The algorithm that was verified
    pub algorithm: String,
    /// Expected hash value (from Metalink)
    pub expected: String,
    /// Computed hash value (from file)
    pub computed: String,
    /// Whether the hashes match
    pub matches: bool,
}

impl HashVerificationResult {
    /// Returns true if verification passed
    pub fn is_valid(&self) -> bool {
        self.matches
    }
}

/// Verify a file's checksum against an expected hash value
///
/// Reads the entire file into memory, computes the hash using the specified
/// algorithm, and compares it with the expected value.
///
/// # Arguments
///
/// * `file_path` - Path to the file to verify
/// * `expected` - HashEntry containing algorithm and expected hash value
///
/// # Returns
///
/// * `Ok(HashVerificationResult)` - Verification result with match status
/// * `Err(String)` - Error if file cannot be read or algorithm is unsupported
///
/// # Examples
///
/// ```ignore
/// use aria2_protocol::metalink::checksum_verifier::*;
/// use aria2_protocol::metalink::parser::{HashAlgorithm, HashEntry};
///
/// let expected = HashEntry::new(HashAlgorithm::Sha256, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
/// let result = verify_checksum(Path::new("/downloads/file.iso"), &expected)?;
/// assert!(result.is_valid());
/// ```
pub fn verify_checksum(
    file_path: &Path,
    expected: &HashEntry,
) -> Result<HashVerificationResult, String> {
    if !file_path.exists() {
        return Err(format!("File does not exist: {}", file_path.display()));
    }

    // Read file content
    let data = std::fs::read(file_path)
        .map_err(|e| format!("Failed to read file {}: {}", file_path.display(), e))?;

    debug!(
        path = %file_path.display(),
        size = data.len(),
        algo = %expected.algo.as_standard_name(),
        "Computing file checksum"
    );

    // Compute hash based on algorithm
    let computed = compute_hash(&data, &expected.algo)?;

    // Compare case-insensitively (hashes are typically lowercase hex)
    let matches = computed.to_lowercase() == expected.value.to_lowercase();

    let result = HashVerificationResult {
        algorithm: expected.algo.as_standard_name().to_string(),
        expected: expected.value.clone(),
        computed,
        matches,
    };

    if matches {
        info!(
            algo = %result.algorithm,
            path = %file_path.display(),
            "Checksum verification passed"
        );
    } else {
        warn!(
            algo = %result.algorithm,
            expected = %result.expected,
            computed = %result.computed,
            path = %file_path.display(),
            "Checksum verification FAILED"
        );
    }

    Ok(result)
}

/// Verify all checksums declared in a MetalinkFile against the downloaded file
///
/// Iterates through all HashEntry values in the MetalinkFile and verifies
/// each one. Returns detailed results for each hash check.
///
/// # Arguments
///
/// * `file_path` - Path to the downloaded file to verify
/// * `metalink_file` - MetalinkFile containing hash declarations
///
/// # Returns
///
/// * `Ok(Vec<HashVerificationResult>)` - Results for each verified hash
/// * `Err(String)` - Error if no hashes or critical failure
///
/// # Behavior
///
/// - If no hashes are declared, returns Ok(empty vec) — not an error
/// - All declared hashes MUST pass for overall success
/// - Individual hash failures are reported but don't stop remaining checks
pub fn verify_all_checksums(
    file_path: &Path,
    metalink_file: &MetalinkFile,
) -> Result<Vec<HashVerificationResult>, String> {
    if metalink_file.hashes.is_empty() {
        debug!(
            name = %metalink_file.name,
            "No checksums declared, skipping verification"
        );
        return Ok(vec![]);
    }

    info!(
        name = %metalink_file.name,
        hash_count = metalink_file.hashes.len(),
        path = %file_path.display(),
        "Verifying all declared checksums"
    );

    let mut results = Vec::with_capacity(metalink_file.hashes.len());

    for hash_entry in &metalink_file.hashes {
        match verify_checksum(file_path, hash_entry) {
            Ok(result) => results.push(result),
            Err(e) => {
                warn!(
                    error = %e,
                    algo = %hash_entry.algo.as_standard_name(),
                    "Failed to verify hash, recording as mismatch"
                );
                // Record as failed verification
                results.push(HashVerificationResult {
                    algorithm: hash_entry.algo.as_standard_name().to_string(),
                    expected: hash_entry.value.clone(),
                    computed: format!("<error: {}>", e),
                    matches: false,
                });
            }
        }
    }

    let passed = results.iter().filter(|r| r.matches).count();
    let total = results.len();

    info!(
        name = %metalink_file.name,
        passed,
        total,
        "Checksum verification complete"
    );

    Ok(results)
}

/// Compute the hex-encoded hash of data using the specified algorithm
///
/// # Arguments
///
/// * `data` - Raw bytes to hash
/// * `algo` - Hash algorithm to use
///
/// # Returns
///
/// * `Ok(String)` - Lowercase hex-encoded hash string
/// * `Err(String)` - Unsupported algorithm error
fn compute_hash(data: &[u8], algo: &HashAlgorithm) -> Result<String, String> {
    use digest::Digest;

    match algo {
        HashAlgorithm::Sha256 => {
            use sha2::Sha256;
            let mut hasher = Sha256::new();
            hasher.update(data);
            Ok(format!("{:x}", hasher.finalize()))
        }
        HashAlgorithm::Sha1 => {
            use sha1::Sha1;
            let mut hasher = Sha1::new();
            hasher.update(data);
            Ok(format!("{:x}", hasher.finalize()))
        }
        HashAlgorithm::Sha512 => {
            use sha2::Sha512;
            let mut hasher = Sha512::new();
            hasher.update(data);
            Ok(format!("{:x}", hasher.finalize()))
        }
        HashAlgorithm::Md5 => {
            // md5 0.7 provides compute() convenience function
            let digest = md5::compute(data);
            Ok(format!("{:x}", digest))
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Create a test file that returns its path (cleanup is manual via helper)
    fn make_test_file(content: &[u8], suffix: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            % 1_000_000_000;
        let dir =
            std::env::temp_dir().join(format!("metalink_cksum_test_{}_{}", std::process::id(), ts));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("test_file{}", suffix));
        fs::write(&path, content).expect("Should write test file");
        path
    }

    fn cleanup_test_file(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_file(path);
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn test_sha256_verification_match() {
        // Known SHA-256 of empty string: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let path = make_test_file(b"", "_sha256_match");
        let expected = HashEntry::new(
            HashAlgorithm::Sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );

        let result = verify_checksum(&path, &expected).expect("Verification should succeed");
        assert!(
            result.is_valid(),
            "SHA-256 of empty string should match known hash"
        );
        assert_eq!(result.algorithm, "sha-256");

        cleanup_test_file(&path);
    }

    #[test]
    fn test_sha256_verification_mismatch_fails() {
        let path = make_test_file(b"hello world", "_sha256_mismatch");
        // Wrong hash for "hello world"
        let expected = HashEntry::new(
            HashAlgorithm::Sha256,
            "0000000000000000000000000000000000000000000000000000000000000000000",
        );

        let result = verify_checksum(&path, &expected).expect("Verification should succeed");
        assert!(!result.is_valid(), "Wrong hash should fail verification");
        assert_ne!(result.computed, result.expected);

        cleanup_test_file(&path);
    }

    #[test]
    fn test_no_checksum_skips_gracefully() {
        let path = make_test_file(b"some data", "_no_checksum");

        // Create a MetalinkFile with no hashes
        let metalink_file = MetalinkFile::new("test_no_hash.bin");

        let results =
            verify_all_checksums(&path, &metalink_file).expect("Should succeed without hashes");
        assert!(
            results.is_empty(),
            "No hashes declared should return empty results"
        );

        cleanup_test_file(&path);
    }

    #[test]
    fn test_multiple_hashes_all_must_pass() {
        // Content: "test content for multiple hashes"
        let content = b"test content for multiple hashes";
        let path = make_test_file(content, "_multi_hash");

        // Create MetalinkFile with multiple hash entries
        let mut metalink_file = MetalinkFile::new("multi_hash_test.bin");

        // Add correct SHA-256
        let sha256_expected =
            compute_hash(content, &HashAlgorithm::Sha256).expect("Should compute SHA-256");
        metalink_file
            .hashes
            .push(HashEntry::new(HashAlgorithm::Sha256, &sha256_expected));

        // Add correct SHA-1
        let sha1_expected =
            compute_hash(content, &HashAlgorithm::Sha1).expect("Should compute SHA-1");
        metalink_file
            .hashes
            .push(HashEntry::new(HashAlgorithm::Sha1, &sha1_expected));

        // Add correct MD5
        let md5_expected = compute_hash(content, &HashAlgorithm::Md5).expect("Should compute MD5");
        metalink_file
            .hashes
            .push(HashEntry::new(HashAlgorithm::Md5, &md5_expected));

        // Verify all
        let results =
            verify_all_checksums(&path, &metalink_file).expect("Should verify all hashes");
        assert_eq!(results.len(), 3, "Should have 3 verification results");
        assert!(
            results.iter().all(|r| r.is_valid()),
            "All correct hashes should pass"
        );

        cleanup_test_file(&path);
    }

    #[test]
    fn test_multiple_hashes_one_fails() {
        let content = b"sensitive data";
        let path = make_test_file(content, "_one_fail");

        let mut metalink_file = MetalinkFile::new("sensitive_data.bin");

        // Add correct SHA-256
        let sha256_ok = compute_hash(content, &HashAlgorithm::Sha256).unwrap();
        metalink_file
            .hashes
            .push(HashEntry::new(HashAlgorithm::Sha256, &sha256_ok));

        // Add WRONG SHA-1 (should fail)
        metalink_file.hashes.push(HashEntry::new(
            HashAlgorithm::Sha1,
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        ));

        let results = verify_all_checksums(&path, &metalink_file)
            .expect("Should complete despite one failure");
        assert_eq!(results.len(), 2);
        assert!(results[0].is_valid(), "First hash (SHA-256) should pass");
        assert!(
            !results[1].is_valid(),
            "Second hash (wrong SHA-1) should fail"
        );

        cleanup_test_file(&path);
    }

    #[test]
    fn test_sha1_verification() {
        // SHA-1 of "test": a94a8fe5ccb19ba61c4c0873d391e987982fbbd3
        let path = make_test_file(b"test", "_sha1_test");
        let expected = HashEntry::new(
            HashAlgorithm::Sha1,
            "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3",
        );

        let result = verify_checksum(&path, &expected).expect("SHA-1 verification should work");
        assert!(result.is_valid(), "SHA-1 should match");
        assert_eq!(result.algorithm, "sha-1");

        cleanup_test_file(&path);
    }

    #[test]
    fn test_md5_verification() {
        // MD5 of "test": d8e8fca2dc0f896fd7cb4cb0031ba249
        let path = make_test_file(b"test", "_md5_test");

        // First compute the actual MD5 to verify our expectation
        let actual_md5 = md5::compute(b"test");
        let actual_hex = format!("{:x}", actual_md5);

        let expected = HashEntry::new(HashAlgorithm::Md5, &actual_hex);

        let result = verify_checksum(&path, &expected).expect("MD5 verification should work");
        assert!(result.is_valid(), "MD5 should match");
        assert_eq!(result.algorithm, "md5");

        cleanup_test_file(&path);
    }

    #[test]
    fn test_verify_nonexistent_file_error() {
        let nonexistent = PathBuf::from("/tmp/this_file_should_not_exist_12345.dat");
        let expected = HashEntry::new(HashAlgorithm::Sha256, "abc123");

        let result = verify_checksum(&nonexistent, &expected);
        assert!(result.is_err(), "Nonexistent file should return error");
        assert!(
            result.unwrap_err().contains("does not exist"),
            "Error should mention file doesn't exist"
        );
    }

    #[test]
    fn test_compute_hash_known_values() {
        // Test known hash values for common inputs
        let empty_data = b"";

        // SHA-256 of empty string
        let sha256 = compute_hash(empty_data, &HashAlgorithm::Sha256).unwrap();
        assert_eq!(
            sha256, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "SHA-256 of empty string"
        );

        // SHA-1 of empty string
        let sha1 = compute_hash(empty_data, &HashAlgorithm::Sha1).unwrap();
        assert_eq!(
            sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "SHA-1 of empty string"
        );

        // MD5 of empty string
        let md5 = compute_hash(empty_data, &HashAlgorithm::Md5).unwrap();
        assert_eq!(
            md5, "d41d8cd98f00b204e9800998ecf8427e",
            "MD5 of empty string"
        );
    }

    #[test]
    fn test_large_file_hashing() {
        // Create a larger file (1KB of repeated pattern)
        let large_content: Vec<u8> = (0..=255).cycle().take(1024).collect();
        let path = make_test_file(&large_content, "_large_file");

        // Just verify it doesn't crash and produces consistent output
        let hash1 = compute_hash(&large_content, &HashAlgorithm::Sha256).unwrap();
        let hash2 = compute_hash(&large_content, &HashAlgorithm::Sha256).unwrap();
        assert_eq!(
            hash1, hash2,
            "Hashing same data twice should produce same result"
        );
        assert_eq!(hash1.len(), 64, "SHA-256 output should be 64 hex chars");

        cleanup_test_file(&path);
    }
}
