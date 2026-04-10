//! Session Codec - Serialization/deserialization for SessionEntry
//!
//! This module provides a clean API layer for encoding/decoding SessionEntry objects.
//! It delegates to the core logic in [`crate::session::session_entry`] while adding
//! validation, GID utilities, and helper functions.
//!
//! # Architecture
//!
//! ```
//! session_codec.rs (this file)
//!   ├── encode()      → delegates to SessionEntry::serialize()
//!   ├── decode()      → delegates to SessionEntry::deserialize_line()
//!   ├── validate()    → structural integrity checks
//!   └── helpers       → GID encoding/decoding, options parsing
//!
//! session_entry.rs (core logic)
//!   ├── serialize()        → full format output
//!   ├── deserialize_line() → parser with all field support
//!   ├── escape_uri()       → URI escaping
//!   └── unescape_uri()     → URI unescaping
//! ```

use std::collections::HashMap;

use crate::error::{Aria2Error, Result};
use crate::session::session_entry::{self, SessionEntry};

/// Session Codec for encoding/decoding SessionEntry objects
///
/// Provides validated serialization with utility functions.
/// All core encoding/decoding logic delegates to [`SessionEntry`].
pub struct SessionCodec;

impl SessionCodec {
    /// Encode a SessionEntry to its text serialization format
    ///
    /// Delegates to [`SessionEntry::serialize()`] which produces the standard
    /// aria2 session file format with all fields.
    ///
    /// # Arguments
    /// * `entry` - The SessionEntry to encode
    ///
    /// # Returns
    /// * The serialized text representation (including trailing newline)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let entry = SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
    /// let text = SessionCodec::encode(&entry);
    /// assert!(text.contains("http://example.com/file.zip"));
    /// assert!(text.contains("GID="));
    /// ```
    pub fn encode(entry: &SessionEntry) -> String {
        entry.serialize()
    }

    /// Decode a single session entry from text format
    ///
    /// Delegates to [`SessionEntry::deserialize_line()`] which handles
    /// all fields including BT-specific data, progress tracking, etc.
    ///
    /// # Arguments
    /// * `text` - Multi-line string containing one entry's data
    ///
    /// # Returns
    /// * `Ok(SessionEntry)` - The decoded entry
    /// * `Err(Aria2Error)` - If parsing fails
    pub fn decode(text: &str) -> Result<SessionEntry> {
        SessionEntry::deserialize_line(text)
    }

    /// Encode a GID value to hex string format (matching aria2 format)
    ///
    /// # Arguments
    /// * `gid` - The numeric GID
    ///
    /// # Returns
    /// * Hex-formatted string (e.g., "d270c8a2")
    pub fn encode_gid(gid: u64) -> String {
        format!("{:x}", gid)
    }

    /// Decode a GID from hex string format
    ///
    /// # Arguments
    /// * `value` - Hex string (with or without "0x" prefix)
    ///
    /// # Returns
    /// * `Ok(u64)` - Decoded GID value
    /// * `Err(Aria2Error)` - If parsing fails
    pub fn decode_gid(value: &str) -> Result<u64> {
        let cleaned = value.trim_start_matches("0x").trim_start_matches("0X");
        u64::from_str_radix(cleaned, 16)
            .map_err(|e| Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Invalid GID format: {} ({})",
                value, e
            ))))
    }

    /// Validate that an encoded entry is well-formed
    ///
    /// Checks basic structural integrity of a serialized entry.
    ///
    /// # Arguments
    /// * `text` - The serialized text to validate
    ///
    /// # Returns
    /// * `Ok(())` if valid
    /// * `Err(Aria2Error)` describing the validation error
    pub fn validate_encoded(text: &str) -> Result<()> {
        let lines: Vec<&str> = text.lines().collect();

        if lines.is_empty() {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config("Empty entry".to_string()),
            ));
        }

        // First line must contain URIs (not starting with space)
        let first_line = lines[0].trim();
        if first_line.is_empty() {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config(
                    "First line must contain URIs".to_string(),
                ),
            ));
        }

        // Should have at least a GID line
        let has_gid = lines.iter().any(|l| l.trim().starts_with("GID="));
        if !has_gid {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config(
                    "Missing GID field".to_string(),
                ),
            ));
        }

        Ok(())
    }

    /// Encode options HashMap to vector of "key=value" strings
    ///
    /// Filters out empty values.
    ///
    /// # Arguments
    /// * `options` - The options map to encode
    ///
    /// # Returns
    /// * Vector of "key=value" strings
    pub fn encode_options(options: &HashMap<String, String>) -> Vec<String> {
        options
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{}={}", k, v))
            .collect()
    }

    /// Decode options from key=value lines
    ///
    /// # Arguments
    /// * `lines` - Iterator over text lines
    ///
    /// * HashMap containing decoded options
    pub fn decode_options<'a, I>(lines: I) -> HashMap<String, String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut options = HashMap::new();

        for line in lines {
            let line = line.trim();
            if let Some(eq_pos) = line.find('=') {
                let key = &line[..eq_pos];
                let value = &line[eq_pos + 1..];
                options.insert(key.to_string(), value.to_string());
            }
        }

        options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_delegates_to_serialize() {
        let entry = SessionEntry::new(
            0x12345678,
            vec!["http://example.com/file.zip".to_string()],
        );
        let encoded = SessionCodec::encode(&entry);

        // Should produce same output as serialize()
        assert_eq!(encoded, entry.serialize());
        assert!(encoded.contains("http://example.com/file.zip"));
        assert!(encoded.contains("GID="));
    }

    #[test]
    fn test_decode_delegates_to_deserialize_line() {
        let text = "http://example.com/file.zip\n GID=12345678\n TOTAL_LENGTH=1000\n";
        let entry = SessionCodec::decode(text).unwrap();

        // Should produce same result as deserialize_line()
        let expected = SessionEntry::deserialize_line(text).unwrap();
        assert_eq!(entry.gid, expected.gid);
        assert_eq!(entry.uris, expected.uris);
        assert_eq!(entry.total_length, expected.total_length);
    }

    #[test]
    fn test_roundtrip_via_codec() {
        let mut original = SessionEntry::new(
            0xDEADBEEF,
            vec![
                "http://primary.com/file.zip".to_string(),
                "http://mirror.com/file.zip".to_string(),
            ],
        );
        original.paused = true;
        original.total_length = 999999;
        original.completed_length = 500000;
        original.options.insert("split".to_string(), "8".to_string());

        let encoded = SessionCodec::encode(&original);
        let decoded = SessionCodec::decode(&encoded).unwrap();

        assert_eq!(decoded.gid, original.gid);
        assert_eq!(decoded.uris.len(), original.uris.len());
        assert_eq!(decoded.paused, original.paused);
        assert_eq!(decoded.total_length, original.total_length);
        assert_eq!(decoded.completed_length, original.completed_length);
    }

    #[test]
    fn test_encode_gid_format() {
        let gid = SessionCodec::encode_gid(0xABCDEF01);
        assert_eq!(gid, "abcdef01");
    }

    #[test]
    fn test_decode_gid_valid() {
        let gid = SessionCodec::decode_gid("d270c8a2").unwrap();
        assert_eq!(gid, 0xd270c8a2);
    }

    #[test]
    fn test_decode_gid_with_prefix() {
        let gid = SessionCodec::decode_gid("0xABCDEF").unwrap();
        assert_eq!(gid, 0xABCDEF);
    }

    #[test]
    fn test_validate_encoded_valid() {
        let text = "http://example.com/f.zip\n GID=1";
        assert!(SessionCodec::validate_encoded(text).is_ok());
    }

    #[test]
    fn test_validate_encoded_no_uri() {
        let text = "\n GID=1";
        assert!(SessionCodec::validate_encoded(text).is_err());
    }

    #[test]
    fn test_validate_encoded_missing_gid() {
        let text = "http://example.com/f.zip\n split=4";
        assert!(SessionCodec::validate_encoded(text).is_err());
    }

    #[test]
    fn test_encode_options_filters_empty() {
        let mut options = HashMap::new();
        options.insert("split".to_string(), "4".to_string());
        options.insert("dir".to_string(), String::new()); // empty value
        options.insert("out".to_string(), "file.bin".to_string());

        let encoded = SessionCodec::encode_options(&options);
        assert_eq!(encoded.len(), 2); // only non-empty values
        assert!(encoded.iter().any(|s| s == "split=4"));
        assert!(encoded.iter().any(|s| s == "out=file.bin"));
    }

    #[test]
    fn test_decode_options_from_lines() {
        let lines = vec!["split=4", "dir=/downloads", "out=file.bin"];
        let options = SessionCodec::decode_options(lines);

        assert_eq!(options.get("split").unwrap(), "4");
        assert_eq!(options.get("dir").unwrap(), "/downloads");
        assert_eq!(options.get("out").unwrap(), "file.bin");
    }
}
