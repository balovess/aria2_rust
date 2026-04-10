//! RPC Handler Utilities - Common helper functions and constants
//!
//! This module provides shared utilities for RPC handler implementations,
//! extracted from rpc_handlers.rs to improve modularity.
//!
//! # Features
//!
//! - Option key validation constants
//! - Response formatting helpers
//! - Status filtering utilities
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/RpcMethodImpl.cc` - Shared utility functions

use std::collections::HashMap;

/// Valid option keys accepted by `aria2.changeOption`.
///
/// Only these keys are allowed when changing per-task options via RPC.
/// Any other key will result in an InvalidParams error.
pub const VALID_OPTION_KEYS: &[&str] = &[
    "split",
    "max-connection-per-server",
    "max-download-limit",
    "max-upload-limit",
    "dir",
    "out",
    "seed-time",
    "seed-ratio",
    "bt-force-encrypt",
    "bt-require-crypto",
    "enable-dht",
    "dht-listen-port",
    "enable-public-trackers",
    "bt-piece-selection-strategy",
    "bt-endgame-threshold",
    "max-retries",
    "retry-wait",
    "http-proxy",
    "dht-file-path",
    "bt-max-upload-slots",
    "bt-optimistic-unchoke-interval",
    "bt-snubbed-timeout",
];

/// Validate that all provided option keys are in the whitelist
///
/// # Arguments
/// * `keys` - Iterator of option key strings to validate
///
/// # Returns
/// * `Ok(())` if all keys are valid
/// * `Err(String)` with the first invalid key found
pub fn validate_option_keys<'a, I>(keys: I) -> Result<(), String>
where
    I: IntoIterator<Item = &'a str>,
{
    for key in keys {
        if !VALID_OPTION_KEYS.contains(&key) {
            return Err(format!("Unknown option: {}", key));
        }
    }
    Ok(())
}

/// Generate session ID based on current timestamp
///
/// Creates a unique session identifier using nanosecond precision.
///
/// # Returns
/// * Session ID string (format: "session-<hex_timestamp>")
pub fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format!("session-{:x}", d.as_nanos()))
        .unwrap_or_else(|_| "session-unknown".to_string())
}

/// Build version info response
///
/// Returns standardized version information object.
///
/// # Returns
/// * JSON value containing version and enabled features
pub fn build_version_info() -> serde_json::Value {
    serde_json::json!({
        "version": "1.37.0-Rust",
        "enabledFeatures": ["http", "https", "ftp", "bittorrent", "metalink", "sftp"],
        "session": "aria2-rpc"
    })
}

/// Format session summary for logging/display
///
/// Creates a human-readable summary of the current session state.
///
/// # Arguments
/// * `active_count` - Number of active downloads
/// * `waiting_count` - Number of waiting downloads
/// * `stopped_count` - Number of stopped downloads
///
/// # Returns
/// * Formatted summary string
pub fn format_session_summary(
    active_count: usize,
    waiting_count: usize,
    stopped_count: usize,
) -> String {
    format!(
        "Session Summary: {} active, {} waiting, {} stopped",
        active_count, waiting_count, stopped_count
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_option_keys_contains_common() {
        assert!(VALID_OPTION_KEYS.contains(&"dir"));
        assert!(VALID_OPTION_KEYS.contains(&"out"));
        assert!(VALID_OPTION_KEYS.contains(&"split"));
        assert!(VALID_OPTION_KEYS.contains(&"max-retries"));
    }

    #[test]
    fn test_validate_option_keys_all_valid() {
        let keys = vec!["dir", "split", "max-retries"];
        assert!(validate_option_keys(keys.iter().map(|s| s.as_str())).is_ok());
    }

    #[test]
    fn test_validate_option_keys_invalid_key() {
        let keys = vec!["invalid-option"];
        assert!(validate_option_keys(keys.iter().map(|s| s.as_str())).is_err());
    }

    #[test]
    fn test_generate_session_id_format() {
        let id = generate_session_id();
        assert!(id.starts_with("session-"));
        assert!(id.len() > 8);
    }

    #[test]
    fn test_build_version_info_structure() {
        let info = build_version_info();
        assert!(info.get("version").is_some());
        assert!(info.get("enabledFeatures").is_some());
        assert!(info.get("session").is_some());
    }

    #[test]
    fn test_format_session_summary() {
        let summary = format_session_summary(5, 3, 2);
        assert!(summary.contains("5 active"));
        assert!(summary.contains("3 waiting"));
        assert!(summary.contains("2 stopped"));
    }
}
