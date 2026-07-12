//! Test Helper Functions for aria2 crate
//!
//! Provides common utilities for integration testing:
//! - Download completion waiting
//! - File content assertion
//! - Test file cleanup
//! - Mock server management

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Wait for a file to be created with timeout.
///
/// # Arguments
/// * `path` - Path to the file to wait for
/// * `timeout` - Maximum time to wait
///
/// # Returns
/// * `true` if file was created within timeout
/// * `false` if timeout elapsed without file creation
pub fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Async version of wait_for_file for tokio runtime.
pub async fn wait_for_file_async(path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Wait for a download to complete by polling the output file existence and size.
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

/// Assert that a file exists and its contents match expected bytes exactly.
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

/// Assert that a file exists and has at least the expected minimum size.
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
pub fn cleanup_test_files(dir: &Path, pattern: &str) -> usize {
    let mut count = 0;
    if dir.exists() && dir.is_dir() && let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(file_name) = path.file_name() {
                let name = file_name.to_string_lossy();
                if (name.contains(pattern) || name.ends_with(pattern))
                    && std::fs::remove_file(&path).is_ok()
                {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Create a temporary directory for testing.
pub fn create_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("Failed to create temp directory")
}

/// Get the path to the test binary (aria2c).
pub fn get_binary_path() -> PathBuf {
    let mut path = std::env::current_exe().expect("Failed to get current exe path");
    path.pop(); // Remove test executable name
    path.pop(); // Remove 'deps'
    path.push("aria2c");

    #[cfg(windows)]
    path.set_extension("exe");

    path
}

/// Check if a process is running by PID.
#[cfg(windows)]
pub fn is_process_running(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.contains(&pid.to_string())
    } else {
        false
    }
}

#[cfg(unix)]
pub fn is_process_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Stop a process by PID.
#[cfg(windows)]
pub fn stop_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
}

#[cfg(unix)]
pub fn stop_process(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .output();
}

/// Generate deterministic test data of given size.
pub fn generate_test_data(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
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
    fn test_create_temp_dir() {
        let dir = create_temp_dir();
        assert!(dir.path().exists());
    }

    #[test]
    fn test_generate_test_data() {
        let d1 = generate_test_data(100, 0x42);
        let d2 = generate_test_data(100, 0x42);
        assert_eq!(d1, d2);
    }

    #[test]
    fn test_build_url() {
        assert_eq!(
            build_url("http://localhost:8080", "/file"),
            "http://localhost:8080/file"
        );
    }

    #[test]
    fn test_get_binary_path() {
        let path = get_binary_path();
        assert!(path.to_string_lossy().contains("aria2c"));
    }
}