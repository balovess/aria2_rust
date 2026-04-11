//! Shared test utilities for deep E2E tests
//!
//! Provides common helpers: temp directories, file assertions, polling, test data generation.

use std::path::Path;

/// Create a temporary directory that auto-cleans on Drop
pub fn setup_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("Failed to create temp directory")
}

/// Assert file exists and contents match expected bytes exactly
pub fn assert_file_contents(path: &Path, expected: &[u8]) {
    let actual = std::fs::read(path).unwrap_or_default();
    assert_eq!(
        actual,
        expected,
        "File content mismatch at {:?}: expected {} bytes, got {} bytes",
        path,
        expected.len(),
        actual.len()
    );
}

/// Assert file exists and its SHA256 matches the expected hex string
#[allow(dead_code)]
pub fn assert_file_sha256(path: &Path, expected_hex: &str) {
    use sha2::{Digest, Sha256};

    let data = std::fs::read(path).unwrap_or_default();
    let hash = Sha256::digest(&data);
    let hex = hex::encode(hash);
    assert_eq!(
        hex, expected_hex,
        "SHA256 mismatch at {:?}: expected {}, got {}",
        path, expected_hex, hex
    );
}

/// Assert download completed successfully with reasonable minimum size
#[allow(dead_code)]
pub fn assert_download_completed(output_path: &Path, expected_min_size: usize) {
    assert!(
        output_path.exists(),
        "Output file missing: {:?}",
        output_path
    );
    let size = std::fs::metadata(output_path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    assert!(
        size >= expected_min_size,
        "File too small: {} bytes (expected >= {}) at {:?}",
        size,
        expected_min_size,
        output_path
    );
}

/// Poll a condition with timeout for async assertions.
///
/// Returns Some(T) when check() returns Some within the timeout, None otherwise.
pub async fn wait_for<F, T>(timeout_secs: u64, mut check: F) -> Option<T>
where
    F: FnMut() -> Option<T>,
{
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        if let Some(result) = check() {
            return Some(result);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    None
}

/// Generate deterministic test data of given size (reproducible across runs).
///
/// Each byte is `(i + seed) % 256`, producing predictable patterns.
pub fn generate_test_data(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// Generate a simple HTTP URL from base + path
pub fn make_url(base: &str, path: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{}{}", trimmed, path)
    } else {
        format!("{}/{}", trimmed, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_temp_dir_creates_and_cleans() {
        let dir = setup_temp_dir();
        let path = dir.path();
        assert!(path.exists(), "Temp dir should exist");

        // Write a file to verify it's writable
        std::fs::write(path.join("test.txt"), "hello").unwrap();
        assert!(path.join("test.txt").exists());
        // Dir will be cleaned on Drop
    }

    #[test]
    fn test_assert_file_contents_match() {
        let dir = setup_temp_dir();
        let f = dir.path().join("match_test.bin");
        let data = vec![1, 2, 3, 4, 5];
        std::fs::write(&f, &data).unwrap();
        assert_file_contents(&f, &data); // should not panic
    }

    #[test]
    fn test_assert_file_contents_mismatch_panics() {
        let dir = setup_temp_dir();
        let f = dir.path().join("mismatch_test.bin");
        std::fs::write(&f, &[1, 2, 3]).unwrap();
        // This should panic due to content mismatch
        let result = std::panic::catch_unwind(|| {
            assert_file_contents(&f, &[9, 9, 9]);
        });
        assert!(result.is_err(), "Expected panic on content mismatch");
    }

    #[test]
    fn test_generate_test_data_deterministic() {
        let d1 = generate_test_data(100, 0x42);
        let d2 = generate_test_data(100, 0x42);
        assert_eq!(d1, d2, "Same params should produce same data");

        let d3 = generate_test_data(50, 0x00);
        assert_eq!(d3.len(), 50);
        // First byte should be 0 (0+0), second should be 1 (1+0), etc.
        assert_eq!(d3[0], 0);
        assert_eq!(d3[1], 1);
        assert_eq!(d3[49], 49);
    }

    #[tokio::test]
    async fn test_wait_for_immediate_success() {
        let result = wait_for(2, || Some(42i32)).await;
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn test_wait_for_timeout_returns_none() {
        let result = wait_for(1, || None::<String>).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_wait_for_delayed_success() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        // Spawn a task that sets the flag after 200ms
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            flag_clone.store(true, Ordering::SeqCst);
        });

        let result = wait_for(2, || {
            if flag.load(Ordering::SeqCst) {
                Some(true)
            } else {
                None
            }
        })
        .await;

        assert_eq!(result, Some(true));
    }

    #[test]
    fn test_make_url() {
        assert_eq!(
            make_url("http://localhost:8080", "/file"),
            "http://localhost:8080/file"
        );
        assert_eq!(
            make_url("http://localhost:8080/", "/file"),
            "http://localhost:8080/file"
        );
        assert_eq!(
            make_url("http://localhost:8080/", "file"),
            "http://localhost:8080/file"
        );
    }
}
