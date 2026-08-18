//! RPC Handler Utilities - Common helper functions and constants
//!
//! This module provides shared utilities for RPC handler implementations,
//! extracted from rpc_handlers.rs to improve modularity.
//!
//! # Features
//!
//! - Shared option value normalization
//! - Response formatting helpers
//! - Status filtering utilities
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/RpcMethodImpl.cc` - Shared utility functions

use aria2_core::request::request_group::option_value_to_string;

/// Decode the permissive Base64 stream used by aria2_original's wire
/// adapters. The original decoder skips bytes outside the Base64 alphabet and
/// returns an empty result for malformed padding or incomplete groups.
pub(crate) fn decode_aria2_base64(input: &str) -> Vec<u8> {
    use base64::Engine;

    let filtered: Vec<u8> = input
        .bytes()
        .filter(|byte| {
            matches!(
                byte,
                b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'+'
                    | b'/'
                    | b'='
            )
        })
        .collect();

    if filtered.is_empty() {
        return Vec::new();
    }

    let input = if let Some(eq_pos) = filtered.iter().position(|byte| *byte == b'=') {
        let group_start = eq_pos / 4 * 4;
        let group_end = group_start + 4;
        if group_end > filtered.len()
            || filtered[eq_pos..group_end].iter().any(|byte| *byte != b'=')
        {
            return Vec::new();
        }
        &filtered[..group_end]
    } else if filtered.len() % 4 == 1 {
        // The original decoder ignores one incomplete trailing alphabet byte.
        &filtered[..filtered.len() - 1]
    } else {
        &filtered
    };

    base64::engine::general_purpose::STANDARD
        .decode(input)
        .unwrap_or_default()
}

/// Normalize a map of RPC options to the string-valued map returned by aria2.
pub fn normalize_rpc_options(
    options: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, serde_json::Value> {
    options
        .iter()
        .filter_map(|(key, value)| {
            option_value_to_string(value)
                .map(|value| (key.clone(), serde_json::Value::String(value)))
        })
        .collect()
}

// Preserve the existing helper path while keeping session identity creation
// in the SessionInfo domain type, which is the owner of its wire contract.
pub use crate::types::generate_session_id;

/// Build the same version response used by `aria2.getVersion`.
///
/// This forwarding helper preserves the public Rust utility without creating
/// a second wire shape beside [`crate::types::VersionInfo`].
pub fn build_version_info() -> serde_json::Value {
    crate::types::VersionInfo::from_env().to_json_value()
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
            let new_arr: Vec<serde_json::Value> =
                arr.into_iter().map(to_aria2_wire_format).collect();
            serde_json::Value::Array(new_arr)
        }
        serde_json::Value::Number(n) => serde_json::Value::String(n.to_string()),
        serde_json::Value::Bool(b) => serde_json::Value::String(if b {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        serde_json::Value::String(_) | serde_json::Value::Null => value,
    }
}

/// Split the aria2 RPC authorization token off a JSON-RPC parameter list.
///
/// Mirrors C++ aria2's `rpc::RpcMethod::authorize()` (`src/RpcMethod.cc`):
/// the first *positional* parameter is always treated as the secret token
/// when it is a string starting with `"token:"`, and it is popped from the
/// list so method handlers never see it and their positional argument
/// indices stay correct.
///
/// Object-style params (`{"token": "..."}`) are also recognised for
/// backward compatibility. Nothing is stripped in that case because
/// named-parameter handlers look their arguments up by name, so a stray
/// `token` key cannot shift any index.
///
/// # Returns
/// `(token, stripped_params)` where
/// * `token` — the secret with the `"token:"` prefix removed, if one was found.
/// * `stripped_params` — `Some(params)` only when the input actually had to be
///   rewritten. `None` means the caller can keep using the original params
///   without cloning them.
///
/// # Examples
/// ```
/// use aria2_rpc::rpc_helpers::split_auth_token;
///
/// let (token, stripped) = split_auth_token(&serde_json::json!(["token:s3cr3t", "gid1"]));
/// assert_eq!(token.as_deref(), Some("s3cr3t"));
/// assert_eq!(stripped, Some(serde_json::json!(["gid1"])));
///
/// // No token → nothing to rewrite.
/// let (token, stripped) = split_auth_token(&serde_json::json!(["gid1"]));
/// assert_eq!(token, None);
/// assert_eq!(stripped, None);
/// ```
pub fn split_auth_token(params: &serde_json::Value) -> (Option<String>, Option<serde_json::Value>) {
    match params {
        serde_json::Value::Array(arr) => {
            let token = arr
                .first()
                .and_then(|v| v.as_str())
                .and_then(|s| s.strip_prefix("token:"))
                .map(str::to_string);
            match token {
                Some(t) => {
                    let mut rest = arr.clone();
                    rest.remove(0);
                    (Some(t), Some(serde_json::Value::Array(rest)))
                }
                None => (None, None),
            }
        }
        other => (
            other
                .get("token")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            None,
        ),
    }
}

/// Consume a parameter value while removing a leading `token:` entry.
///
/// Server-side parsers already own their request DOM. This variant preserves
/// the borrowed helper's wire semantics without cloning the whole positional
/// parameter array before dispatch.
pub(crate) fn split_auth_token_owned(
    params: serde_json::Value,
) -> (Option<String>, serde_json::Value) {
    match params {
        serde_json::Value::Array(mut arr) => {
            let token = arr
                .first()
                .and_then(|value| value.as_str())
                .and_then(|value| value.strip_prefix("token:"))
                .map(str::to_owned);
            if token.is_some() {
                arr.remove(0);
            }
            (token, serde_json::Value::Array(arr))
        }
        params => {
            let token = params
                .get("token")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            (token, params)
        }
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
    fn test_build_version_info_structure() {
        let info = build_version_info();
        assert_eq!(info, crate::types::VersionInfo::from_env().to_json_value());
        assert!(info.get("version").is_some());
        assert!(info.get("enabledFeatures").is_some());
        assert!(info.get("session").is_none());
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
        assert!(
            output["creationDate"].is_number(),
            "creationDate should remain a number"
        );
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
