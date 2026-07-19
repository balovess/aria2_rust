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

/// Fields that original aria2 keeps as native JSON numbers (not strings).
/// These are exceptions to the general rule that all numbers → strings.
const NATIVE_NUMERIC_FIELDS: &[&str] = &["creationDate"];

/// Post-process a JSON value to match original aria2 wire format.
///
/// Converts all numbers to strings and booleans to "true"/"false" strings,
/// except for fields listed in [`NATIVE_NUMERIC_FIELDS`].
pub fn to_aria2_wire_format(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if NATIVE_NUMERIC_FIELDS.contains(&k.as_str()) {
                    new_map.insert(k, v);
                } else {
                    new_map.insert(k, to_aria2_wire_format(v));
                }
            }
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            let new_arr: Vec<serde_json::Value> = arr
                .into_iter()
                .map(to_aria2_wire_format)
                .collect();
            serde_json::Value::Array(new_arr)
        }
        serde_json::Value::Number(n) => serde_json::Value::String(n.to_string()),
        serde_json::Value::Bool(b) => {
            serde_json::Value::String(if b { "true".to_string() } else { "false".to_string() })
        }
        serde_json::Value::String(_) | serde_json::Value::Null => value,
    }
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
        assert!(validate_option_keys(keys.iter().copied()).is_ok());
    }

    #[test]
    fn test_validate_option_keys_invalid_key() {
        let keys = vec!["invalid-option"];
        assert!(validate_option_keys(keys.iter().copied()).is_err());
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

    #[test]
    fn test_to_aria2_wire_format_numbers() {
        let input = serde_json::json!({"a": 123, "b": 456});
        let output = to_aria2_wire_format(input);
        assert_eq!(output["a"].as_str(), Some("123"));
        assert_eq!(output["b"].as_str(), Some("456"));
    }

    #[test]
    fn test_to_aria2_wire_format_bools() {
        let input = serde_json::json!({"x": true, "y": false});
        let output = to_aria2_wire_format(input);
        assert_eq!(output["x"].as_str(), Some("true"));
        assert_eq!(output["y"].as_str(), Some("false"));
    }

    #[test]
    fn test_to_aria2_wire_format_native_numeric() {
        let input = serde_json::json!({"creationDate": 1700000000, "other": 42});
        let output = to_aria2_wire_format(input);
        assert!(output["creationDate"].is_number(), "creationDate should remain a number");
        assert_eq!(output["other"].as_str(), Some("42"));
    }

    #[test]
    fn test_to_aria2_wire_format_bitfield() {
        let input = serde_json::json!({"bitfield": "ff00ff00"});
        let output = to_aria2_wire_format(input);
        assert_eq!(output["bitfield"].as_str(), Some("ff00ff00"));
    }

    #[test]
    fn test_to_aria2_wire_format_piece_fields() {
        let input = serde_json::json!({"pieceLength": 262144, "numPieces": 128});
        let output = to_aria2_wire_format(input);
        assert_eq!(output["pieceLength"].as_str(), Some("262144"));
        assert_eq!(output["numPieces"].as_str(), Some("128"));
    }

    #[test]
    fn test_to_aria2_wire_format_followed_by() {
        let input = serde_json::json!({"followedBy": ["gid-abc", "gid-def"]});
        let output = to_aria2_wire_format(input);
        assert_eq!(output["followedBy"][0].as_str(), Some("gid-abc"));
        assert_eq!(output["followedBy"][1].as_str(), Some("gid-def"));
    }

    #[test]
    fn test_to_aria2_wire_format_complete_status() {
        let input = serde_json::json!({
            "gid": "abc123",
            "totalLength": 1048576,
            "completedLength": 524288,
            "downloadSpeed": 1024000,
            "uploadSpeed": 512000,
            "uploadLength": 0,
            "connections": 4,
            "pieceLength": 16384,
            "numPieces": 64,
            "numSeeders": 2,
            "verifiedLength": 0,
            "verifyIntegrityPending": "false",
            "seeder": "false",
            "infoHash": "abcdef0123456789",
            "belongsTo": "parent-gid",
            "followedBy": ["child1", "child2"],
            "bitfield": "ffff0000",
            "dir": "/downloads",
            "files": [{"index": 1, "path": "/downloads/file.iso", "length": 1048576, "completedLength": 524288, "selected": "true", "uris": [{"uri": "http://example.com/file.iso", "status": "used"}]}]
        });
        let output = to_aria2_wire_format(input);

        // Numeric fields should be strings
        assert_eq!(output["totalLength"].as_str(), Some("1048576"));
        assert_eq!(output["completedLength"].as_str(), Some("524288"));
        assert_eq!(output["downloadSpeed"].as_str(), Some("1024000"));
        assert_eq!(output["uploadSpeed"].as_str(), Some("512000"));
        assert_eq!(output["pieceLength"].as_str(), Some("16384"));
        assert_eq!(output["numPieces"].as_str(), Some("64"));
        assert_eq!(output["numSeeders"].as_str(), Some("2"));

        // String fields should pass through
        assert_eq!(output["gid"].as_str(), Some("abc123"));
        assert_eq!(output["bitfield"].as_str(), Some("ffff0000"));
        assert_eq!(output["infoHash"].as_str(), Some("abcdef0123456789"));
        assert_eq!(output["belongsTo"].as_str(), Some("parent-gid"));
        assert_eq!(output["verifyIntegrityPending"].as_str(), Some("false"));
        assert_eq!(output["seeder"].as_str(), Some("false"));

        // Array passthrough
        assert_eq!(output["followedBy"][0].as_str(), Some("child1"));
        assert_eq!(output["followedBy"][1].as_str(), Some("child2"));

        // Nested objects
        let file = &output["files"][0];
        assert_eq!(file["path"].as_str(), Some("/downloads/file.iso"));
        let uri = &file["uris"][0];
        assert_eq!(uri["uri"].as_str(), Some("http://example.com/file.iso"));
        assert_eq!(uri["status"].as_str(), Some("used"));
    }
}
