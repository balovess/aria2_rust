//! Test Helper Functions
//!
//! Provides common utilities for integration and E2E testing:
//! - Download completion waiting
//! - File content assertion
//! - Test file cleanup
//! - Mock server management

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Wait for a download to complete by polling the output file existence and size.
///
/// # Arguments
/// * `output_path` - Path to the expected output file
/// * `expected_size` - Expected minimum file size in bytes
/// * `timeout_secs` - Maximum time to wait in seconds
///
/// # Returns
/// * `true` if download completed within timeout
/// * `false` if timeout elapsed without completion
pub fn wait_for_download_complete(
    output_path: &Path,
    expected_size: usize,
    timeout_secs: u64,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        if output_path.exists() {
            let size = std::fs::metadata(output_path)
                .map(|m| m.len() as usize)
                .unwrap_or(0);
            if size >= expected_size {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Async version of wait_for_download_complete for tokio runtime.
pub async fn wait_for_download_complete_async(
    output_path: &Path,
    expected_size: usize,
    timeout_secs: u64,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        if output_path.exists() {
            let size = std::fs::metadata(output_path)
                .map(|m| m.len() as usize)
                .unwrap_or(0);
            if size >= expected_size {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Assert that a file exists and its contents match expected bytes exactly.
///
/// # Arguments
/// * `path` - Path to the file to check
/// * `expected` - Expected byte content
///
/// # Panics
/// Panics if file doesn't exist or content doesn't match.
pub fn assert_file_content(path: &Path, expected: &[u8]) {
    assert!(
        path.exists(),
        "File does not exist: {:?}",
        path
    );
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

/// Assert that a file exists and its SHA256 hash matches expected hex string.
///
/// # Arguments
/// * `path` - Path to the file to check
/// * `expected_sha256_hex` - Expected SHA256 hash as hex string
///
/// # Panics
/// Panics if file doesn't exist or hash doesn't match.
pub fn assert_file_sha256(path: &Path, expected_sha256_hex: &str) {
    use sha2::{Digest, Sha256};

    assert!(
        path.exists(),
        "File does not exist: {:?}",
        path
    );
    let data = std::fs::read(path).unwrap_or_default();
    let hash = Sha256::digest(&data);
    let hex = hex::encode(hash);
    assert_eq!(
        hex,
        expected_sha256_hex,
        "SHA256 mismatch at {:?}: expected {}, got {}",
        path,
        expected_sha256_hex,
        hex
    );
}

/// Assert that a file exists and has at least the expected minimum size.
///
/// # Arguments
/// * `path` - Path to the file to check
/// * `min_size` - Minimum expected size in bytes
///
/// # Panics
/// Panics if file doesn't exist or is smaller than min_size.
pub fn assert_file_min_size(path: &Path, min_size: usize) {
    assert!(
        path.exists(),
        "File does not exist: {:?}",
        path
    );
    let size = std::fs::metadata(path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    assert!(
        size >= min_size,
        "File too small at {:?}: {} bytes (expected >= {})",
        path,
        size,
        min_size
    );
}

/// Clean up test files in a directory matching a pattern.
///
/// # Arguments
/// * `dir` - Directory to clean up
/// * `pattern` - Glob pattern for files to remove (e.g., "*.tmp")
///
/// # Returns
/// Number of files removed.
pub fn cleanup_test_files(dir: &Path, pattern: &str) -> usize {
    let mut count = 0;
    if dir.exists() && dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(file_name) = path.file_name() {
                    if file_name.to_string_lossy().contains(pattern)
                        || matches_glob_pattern(&file_name.to_string_lossy(), pattern)
                    {
                        if std::fs::remove_file(&path).is_ok() {
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    count
}

/// Clean up all files in a temporary directory (non-recursive).
///
/// # Arguments
/// * `dir` - Directory to clean up
pub fn cleanup_dir_contents(dir: &Path) {
    if dir.exists() && dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let _ = std::fs::remove_file(&path);
                } else if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
    }
}

/// Simple glob pattern matcher (supports * wildcard only).
fn matches_glob_pattern(text: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return text == pattern;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 2 {
        text.starts_with(parts[0]) && text.ends_with(parts[1])
    } else {
        // More complex patterns - simple contains check
        text.contains(&pattern.replace('*', ""))
    }
}

/// Create a temporary directory for testing.
///
/// Returns a `tempfile::TempDir` that auto-cleans on drop.
pub fn create_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("Failed to create temp directory")
}

/// Create a temporary directory with a specific prefix name.
pub fn create_temp_dir_with_prefix(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("Failed to create temp directory")
}

/// Mock HTTP Server wrapper for testing.
///
/// Provides a simple HTTP server that can serve test files.
pub struct MockServer {
    addr: std::net::SocketAddr,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockServer {
    /// Start a mock HTTP server on a random port.
    ///
    /// The server serves files from the provided `files_dir` directory.
    pub async fn start(files_dir: &Path) -> Self {
        use tokio::net::TcpListener;

        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("Failed to bind mock server");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        let files_dir = files_dir.to_path_buf();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((mut stream, _)) => {
                                Self::handle_connection(&mut stream, &files_dir).await;
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        MockServer {
            addr: actual_addr,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    async fn handle_connection(
        stream: &mut tokio::net::TcpStream,
        files_dir: &Path,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        buf.truncate(n);

        let request_str = String::from_utf8_lossy(&buf);
        let path = request_str
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .unwrap_or("/");

        // Simple file serving
        let file_path = files_dir.join(path.trim_start_matches('/'));
        if file_path.exists() && file_path.is_file() {
            let content = std::fs::read(&file_path).unwrap_or_default();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
                content.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(&content).await;
            let _ = stream.flush().await;
        } else {
            let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    }

    /// Get the base URL of the mock server.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Get the port number of the mock server.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Stop the mock server gracefully.
    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Generate deterministic test data of given size.
///
/// Each byte is `(i + seed) % 256`, producing predictable patterns.
pub fn generate_test_data(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// Generate test data with a specific pattern.
pub fn generate_pattern_data(size: usize, pattern: u8) -> Vec<u8> {
    vec![pattern; size]
}

/// Poll a condition with timeout for async assertions.
///
/// Returns Some(T) when check() returns Some within the timeout, None otherwise.
pub async fn wait_for_condition<F, T>(timeout_secs: u64, mut check: F) -> Option<T>
where
    F: FnMut() -> Option<T>,
{
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        if let Some(result) = check() {
            return Some(result);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Build a URL from base and path components.
pub fn build_url(base: &str, path: &str) -> String {
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
    fn test_generate_test_data_deterministic() {
        let d1 = generate_test_data(100, 0x42);
        let d2 = generate_test_data(100, 0x42);
        assert_eq!(d1, d2, "Same params should produce same data");

        let d3 = generate_test_data(50, 0x00);
        assert_eq!(d3.len(), 50);
        assert_eq!(d3[0], 0);
        assert_eq!(d3[1], 1);
        assert_eq!(d3[49], 49);
    }

    #[test]
    fn test_generate_pattern_data() {
        let data = generate_pattern_data(1024, 0xAB);
        assert_eq!(data.len(), 1024);
        assert!(data.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn test_build_url() {
        assert_eq!(
            build_url("http://localhost:8080", "/file"),
            "http://localhost:8080/file"
        );
        assert_eq!(
            build_url("http://localhost:8080/", "/file"),
            "http://localhost:8080/file"
        );
        assert_eq!(
            build_url("http://localhost:8080/", "file"),
            "http://localhost:8080/file"
        );
    }

    #[test]
    fn test_matches_glob_pattern() {
        assert!(matches_glob_pattern("test.txt", "*"));
        assert!(matches_glob_pattern("test.txt", "*.txt"));
        assert!(matches_glob_pattern("test.txt", "test*"));
        assert!(!matches_glob_pattern("test.txt", "*.bin"));
        assert!(matches_glob_pattern("test.tmp", "*.tmp"));
    }

    #[test]
    fn test_assert_file_content() {
        let dir = create_temp_dir();
        let file = dir.path().join("test.bin");
        let data = vec![1, 2, 3, 4, 5];
        std::fs::write(&file, &data).unwrap();
        assert_file_content(&file, &data);
    }

    #[test]
    fn test_create_temp_dir() {
        let dir = create_temp_dir();
        assert!(dir.path().exists());
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();
        assert!(dir.path().join("test.txt").exists());
    }

    #[test]
    fn test_cleanup_test_files() {
        let dir = create_temp_dir();
        std::fs::write(dir.path().join("a.tmp"), "").unwrap();
        std::fs::write(dir.path().join("b.tmp"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();

        let count = cleanup_test_files(dir.path(), "*.tmp");
        assert_eq!(count, 2);
        assert!(!dir.path().join("a.tmp").exists());
        assert!(!dir.path().join("b.tmp").exists());
        assert!(dir.path().join("c.txt").exists());
    }

    #[tokio::test]
    async fn test_wait_for_condition_immediate() {
        let result = wait_for_condition(2, || Some(42i32)).await;
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn test_wait_for_condition_timeout() {
        let result = wait_for_condition(1, || None::<String>).await;
        assert_eq!(result, None);
    }
}