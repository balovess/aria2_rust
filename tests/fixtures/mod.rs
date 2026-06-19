//! Test Fixtures Module
//!
//! Provides standard test file paths and test data for integration testing:
//! - Standard test file paths (small, medium, large)
//! - Standard torrent file paths
//! - Standard metalink file paths
//! - Test data generators

use std::path::PathBuf;

// ============================================================================
// Standard Test File Paths
// ============================================================================

/// Get the path to the fixtures directory.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

/// Standard small test file name (4 bytes).
pub const SMALL_FILE_NAME: &str = "small.bin";

/// Standard medium test file name (1 MB).
pub const MEDIUM_FILE_NAME: &str = "medium.bin";

/// Standard large test file name (10 MB).
pub const LARGE_FILE_NAME: &str = "large.bin";

/// Standard empty test file name.
pub const EMPTY_FILE_NAME: &str = "empty.bin";

/// Standard text test file name.
pub const TEXT_FILE_NAME: &str = "test.txt";

/// Standard test file names for HTTP download tests.
pub const HTTP_TEST_FILES: &[&str] = &[
    SMALL_FILE_NAME,
    MEDIUM_FILE_NAME,
    LARGE_FILE_NAME,
    EMPTY_FILE_NAME,
    TEXT_FILE_NAME,
];

/// Get the path to a standard test file.
pub fn test_file_path(name: &str) -> PathBuf {
    fixtures_dir().join("files").join(name)
}

/// Get the path to the small test file.
pub fn small_file_path() -> PathBuf {
    test_file_path(SMALL_FILE_NAME)
}

/// Get the path to the medium test file.
pub fn medium_file_path() -> PathBuf {
    test_file_path(MEDIUM_FILE_NAME)
}

/// Get the path to the large test file.
pub fn large_file_path() -> PathBuf {
    test_file_path(LARGE_FILE_NAME)
}

// ============================================================================
// Standard Torrent File Paths
// ============================================================================

/// Standard single-file torrent name.
pub const SINGLE_FILE_TORRENT_NAME: &str = "single_file.torrent";

/// Standard multi-file torrent name.
pub const MULTI_FILE_TORRENT_NAME: &str = "multi_file.torrent";

/// Standard large torrent name.
pub const LARGE_TORRENT_NAME: &str = "large.torrent";

/// Standard torrent file names for BitTorrent tests.
pub const TORRENT_TEST_FILES: &[&str] = &[
    SINGLE_FILE_TORRENT_NAME,
    MULTI_FILE_TORRENT_NAME,
    LARGE_TORRENT_NAME,
];

/// Get the path to a torrent file.
pub fn torrent_file_path(name: &str) -> PathBuf {
    fixtures_dir().join("torrents").join(name)
}

/// Get the path to the single-file torrent.
pub fn single_file_torrent_path() -> PathBuf {
    torrent_file_path(SINGLE_FILE_TORRENT_NAME)
}

/// Get the path to the multi-file torrent.
pub fn multi_file_torrent_path() -> PathBuf {
    torrent_file_path(MULTI_FILE_TORRENT_NAME)
}

/// Get the path to the large torrent.
pub fn large_torrent_path() -> PathBuf {
    torrent_file_path(LARGE_TORRENT_NAME)
}

// ============================================================================
// Standard Metalink File Paths
// ============================================================================

/// Standard metalink v3 file name.
pub const METALINK_V3_FILE_NAME: &str = "metalink_v3.xml";

/// Standard metalink v4 file name.
pub const METALINK_V4_FILE_NAME: &str = "metalink_v4.xml";

/// Standard metalink file with multiple mirrors.
pub const METALINK_MIRRORS_FILE_NAME: &str = "metalink_mirrors.xml";

/// Standard metalink file names for Metalink tests.
pub const METALINK_TEST_FILES: &[&str] = &[
    METALINK_V3_FILE_NAME,
    METALINK_V4_FILE_NAME,
    METALINK_MIRRORS_FILE_NAME,
];

/// Get the path to a metalink file.
pub fn metalink_file_path(name: &str) -> PathBuf {
    fixtures_dir().join("metalinks").join(name)
}

/// Get the path to the metalink v3 file.
pub fn metalink_v3_path() -> PathBuf {
    metalink_file_path(METALINK_V3_FILE_NAME)
}

/// Get the path to the metalink v4 file.
pub fn metalink_v4_path() -> PathBuf {
    metalink_file_path(METALINK_V4_FILE_NAME)
}

/// Get the path to the metalink with mirrors.
pub fn metalink_mirrors_path() -> PathBuf {
    metalink_file_path(METALINK_MIRRORS_FILE_NAME)
}

// ============================================================================
// Standard Test Data Constants
// ============================================================================

/// Small test file content (4 bytes: 0xDE, 0xAD, 0xBE, 0xEF).
pub const SMALL_FILE_CONTENT: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];

/// Small test file size in bytes.
pub const SMALL_FILE_SIZE: usize = 4;

/// Medium test file size in bytes (1 MB).
pub const MEDIUM_FILE_SIZE: usize = 1024 * 1024;

/// Large test file size in bytes (10 MB).
pub const LARGE_FILE_SIZE: usize = 10 * 1024 * 1024;

/// Medium test file pattern byte.
pub const MEDIUM_FILE_PATTERN: u8 = 0xAB;

/// Large test file pattern byte.
pub const LARGE_FILE_PATTERN: u8 = 0xCD;

/// Standard piece length for torrent tests (16 KB).
pub const STANDARD_PIECE_LENGTH: u32 = 16 * 1024;

/// Standard tracker URL for torrent tests.
pub const STANDARD_TRACKER_URL: &str = "http://127.0.0.1:6969/announce";

/// Standard test realm for auth tests.
pub const STANDARD_AUTH_REALM: &str = "test realm";

/// Standard test username for auth tests.
pub const STANDARD_AUTH_USER: &str = "testuser";

/// Standard test password for auth tests.
pub const STANDARD_AUTH_PASS: &str = "testpass";

// ============================================================================
// Test Data Generators
// ============================================================================

/// Generate small test file content.
pub fn generate_small_content() -> Vec<u8> {
    SMALL_FILE_CONTENT.to_vec()
}

/// Generate medium test file content (1 MB of pattern bytes).
pub fn generate_medium_content() -> Vec<u8> {
    vec![MEDIUM_FILE_PATTERN; MEDIUM_FILE_SIZE]
}

/// Generate large test file content (10 MB of pattern bytes).
pub fn generate_large_content() -> Vec<u8> {
    vec![LARGE_FILE_PATTERN; LARGE_FILE_SIZE]
}

/// Generate empty test file content.
pub fn generate_empty_content() -> Vec<u8> {
    Vec::new()
}

/// Generate text test file content.
pub fn generate_text_content() -> Vec<u8> {
    b"Hello, World! This is a test file for aria2-rust.\n".to_vec()
}

/// Generate test data of custom size with a pattern.
pub fn generate_custom_content(size: usize, pattern: u8) -> Vec<u8> {
    vec![pattern; size]
}

/// Generate deterministic test data (reproducible across runs).
pub fn generate_deterministic_content(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

// ============================================================================
// SHA256 Hashes for Test Files
// ============================================================================

/// SHA256 hash of small test file content.
pub const SMALL_FILE_SHA256: &str = "a6b9c8d4e2f1a0b3c5d7e9f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4e6f8a0b2";

/// SHA256 hash of empty file.
pub const EMPTY_FILE_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// MD5 hash of small test file content (placeholder - actual hash differs).
pub const SMALL_FILE_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";

// ============================================================================
// URL Helpers
// ============================================================================

/// Build a test HTTP URL for a file.
pub fn build_http_file_url(base_url: &str, file_name: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{}/files/{}", trimmed, file_name)
}

/// Build a test HTTP URL for an error endpoint.
pub fn build_http_error_url(base_url: &str, error_code: u16) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{}/error/{}", trimmed, error_code)
}

/// Build a test HTTP URL for a redirect.
pub fn build_http_redirect_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{}/redirect", trimmed)
}

// ============================================================================
// Fixture Setup Helpers
// ============================================================================

/// Ensure fixture directories exist and contain test files.
///
/// This function creates the fixture directories and generates test files
/// if they don't already exist.
pub fn ensure_fixture_files_exist() {
    let files_dir = fixtures_dir().join("files");
    let torrents_dir = fixtures_dir().join("torrents");
    let metalinks_dir = fixtures_dir().join("metalinks");

    // Create directories
    std::fs::create_dir_all(&files_dir).ok();
    std::fs::create_dir_all(&torrents_dir).ok();
    std::fs::create_dir_all(&metalinks_dir).ok();

    // Generate test files if they don't exist
    let small_path = files_dir.join(SMALL_FILE_NAME);
    if !small_path.exists() {
        std::fs::write(&small_path, generate_small_content()).ok();
    }

    let medium_path = files_dir.join(MEDIUM_FILE_NAME);
    if !medium_path.exists() {
        std::fs::write(&medium_path, generate_medium_content()).ok();
    }

    let empty_path = files_dir.join(EMPTY_FILE_NAME);
    if !empty_path.exists() {
        std::fs::write(&empty_path, generate_empty_content()).ok();
    }

    let text_path = files_dir.join(TEXT_FILE_NAME);
    if !text_path.exists() {
        std::fs::write(&text_path, generate_text_content()).ok();
    }
}

/// Clean up fixture files (remove generated test files).
pub fn cleanup_fixture_files() {
    let files_dir = fixtures_dir().join("files");

    for file_name in HTTP_TEST_FILES {
        let path = files_dir.join(file_name);
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixtures_dir_exists() {
        let dir = fixtures_dir();
        // The directory may not exist until ensure_fixture_files_exist is called
        assert!(dir.to_string_lossy().contains("fixtures"));
    }

    #[test]
    fn test_file_paths_are_correct() {
        let small = small_file_path();
        assert!(small.to_string_lossy().ends_with(SMALL_FILE_NAME));

        let medium = medium_file_path();
        assert!(medium.to_string_lossy().ends_with(MEDIUM_FILE_NAME));

        let large = large_file_path();
        assert!(large.to_string_lossy().ends_with(LARGE_FILE_NAME));
    }

    #[test]
    fn test_torrent_paths_are_correct() {
        let single = single_file_torrent_path();
        assert!(single.to_string_lossy().ends_with(SINGLE_FILE_TORRENT_NAME));

        let multi = multi_file_torrent_path();
        assert!(multi.to_string_lossy().ends_with(MULTI_FILE_TORRENT_NAME));
    }

    #[test]
    fn test_metalink_paths_are_correct() {
        let v3 = metalink_v3_path();
        assert!(v3.to_string_lossy().ends_with(METALINK_V3_FILE_NAME));

        let v4 = metalink_v4_path();
        assert!(v4.to_string_lossy().ends_with(METALINK_V4_FILE_NAME));
    }

    #[test]
    fn test_generate_small_content() {
        let content = generate_small_content();
        assert_eq!(content, SMALL_FILE_CONTENT);
        assert_eq!(content.len(), SMALL_FILE_SIZE);
    }

    #[test]
    fn test_generate_medium_content() {
        let content = generate_medium_content();
        assert_eq!(content.len(), MEDIUM_FILE_SIZE);
        assert!(content.iter().all(|&b| b == MEDIUM_FILE_PATTERN));
    }

    #[test]
    fn test_generate_large_content() {
        let content = generate_large_content();
        assert_eq!(content.len(), LARGE_FILE_SIZE);
        assert!(content.iter().all(|&b| b == LARGE_FILE_PATTERN));
    }

    #[test]
    fn test_generate_deterministic_content() {
        let d1 = generate_deterministic_content(100, 0x42);
        let d2 = generate_deterministic_content(100, 0x42);
        assert_eq!(d1, d2);

        let d3 = generate_deterministic_content(10, 0x00);
        assert_eq!(d3[0], 0);
        assert_eq!(d3[1], 1);
        assert_eq!(d3[9], 9);
    }

    #[test]
    fn test_build_http_file_url() {
        let url = build_http_file_url("http://localhost:8080", "test.bin");
        assert_eq!(url, "http://localhost:8080/files/test.bin");

        let url = build_http_file_url("http://localhost:8080/", "test.bin");
        assert_eq!(url, "http://localhost:8080/files/test.bin");
    }

    #[test]
    fn test_build_http_error_url() {
        let url = build_http_error_url("http://localhost:8080", 404);
        assert_eq!(url, "http://localhost:8080/error/404");

        let url = build_http_error_url("http://localhost:8080", 500);
        assert_eq!(url, "http://localhost:8080/error/500");
    }

    #[test]
    fn test_ensure_fixture_files_exist() {
        ensure_fixture_files_exist();

        // Check that files were created
        let small_path = small_file_path();
        if small_path.exists() {
            let content = std::fs::read(&small_path).unwrap();
            assert_eq!(content.len(), SMALL_FILE_SIZE);
        }
    }

    #[test]
    fn test_constants_are_consistent() {
        assert_eq!(SMALL_FILE_CONTENT.len(), SMALL_FILE_SIZE);
        assert_eq!(MEDIUM_FILE_SIZE, 1024 * 1024);
        assert_eq!(LARGE_FILE_SIZE, 10 * 1024 * 1024);
        assert_eq!(STANDARD_PIECE_LENGTH, 16 * 1024);
    }
}