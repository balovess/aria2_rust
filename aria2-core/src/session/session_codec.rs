//! Session Codec - Serialization/deserialization for SessionEntry
//!
//! This module handles the conversion between SessionEntry structs and their
//! text-based serialization format used in aria2 session files.
//!
//! # Serialization Format
//!
//! Each entry is serialized as a multi-line block:
//! ```text
//! uri1\turi2\turi3
//!  GID=hex_value
//!  [PAUSE=true]
//!  key=value
//!  TOTAL_LENGTH=...
//!  COMPLETED_LENGTH=...
//!  ...
//! ```
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/SessionSerializer.cc/h` - Format conversion logic

use std::collections::HashMap;

use crate::error::{Aria2Error, Result};
use crate::session::session_entry::SessionEntry;

/// Session Codec for encoding/decoding SessionEntry objects
///
/// Provides methods to convert between SessionEntry and text format.
pub struct SessionCodec;

impl SessionCodec {
    /// Encode a SessionEntry to its text serialization format
    ///
    /// Converts all fields of the entry into the standard aria2 session format.
    /// The output can be written directly to a session file.
    ///
    /// # Arguments
    /// * `entry` - The SessionEntry to encode
    ///
    /// # Returns
    /// * `Ok(String)` - The serialized text representation
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let entry = SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
    /// let text = SessionCodec::encode(&entry)?;
    /// assert!(text.starts_with("http://example.com/file.zip"));
    /// ```
    pub fn encode(entry: &SessionEntry) -> String {
        // Line 1: URIs (tab-separated)
        let mut lines = Vec::new();
        lines.push(entry.uris.join("\t"));

        // Line 2: GID (hex format)
        lines.push(format!("GID={:016x}", entry.gid));

        // Line 3: Pause flag (only if paused)
        if entry.paused {
            lines.push("PAUSE=true".to_string());
        }

        // Lines 4+: Options (key=value pairs)
        for (key, value) in &entry.options {
            if !value.is_empty() {
                lines.push(format!("{}={}", key, value));
            }
        }

        // Progress fields
        if entry.total_length > 0 {
            lines.push(format!("TOTAL_LENGTH={}", entry.total_length));
        }

        if entry.completed_length > 0 {
            lines.push(format!("COMPLETED_LENGTH={}", entry.completed_length));
        }

        if entry.upload_length > 0 {
            lines.push(format!("UPLOAD_LENGTH={}", entry.upload_length));
        }

        // Status field (if not default "active")
        if !entry.status.is_empty() && entry.status != "active" {
            lines.push(format!("STATUS={}", entry.status));
        }

        // Error code (if error status)
        if entry.status == "error" && entry.error_code != 0 {
            lines.push(format!("ERROR_CODE={}", entry.error_code));
        }

        // BitTorrent-specific fields
        if !entry.bitfield.is_empty() {
            lines.push(format!("BITFIELD={}", entry.bitfield));
        }

        if entry.num_pieces > 0 {
            lines.push(format!("NUM_PIECES={}", entry.num_pieces));
        }

        if entry.piece_length > 0 {
            lines.push(format!("PIECE_LENGTH={}", entry.piece_length));
        }

        if !entry.info_hash_hex.is_empty() {
            lines.push(format!("INFO_HASH={}", entry.info_hash_hex));
        }

        if entry.resume_offset > 0 {
            lines.push(format!("RESUME_OFFSET={}", entry.resume_offset));
        }

        lines.join("\n")
    }

    /// Decode a single session entry from text format
    ///
    /// Parses a multi-line block of text and reconstructs a SessionEntry.
    ///
    /// # Arguments
    /// * `text` - The serialized text (may contain multiple entries separated by blank lines)
    ///
    /// # Returns
    /// * `Ok(SessionEntry)` - The decoded entry
    /// * `Err(Aria2Error)` - If parsing fails
    ///
    /// # Format Expected
    ///
    /// ```text
    /// uri1\turi2\turi3          <- First line: URIs (tab-separated)
    /// GID=hex_value             <- Second line: GID
    /// PAUSE=true               <- Optional: pause flag
    /// key=value                <- Options and progress fields
    /// ```
    pub fn decode(text: &str) -> Result<SessionEntry> {
        let mut lines: Vec<&str> = text.lines().collect();

        // Remove trailing empty lines
        while lines.last().map_or(false, |l| l.is_empty()) {
            lines.pop();
        }

        if lines.is_empty() {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config("Empty session entry".to_string()),
            ));
        }

        // Parse first line as URIs
        let uris_str = lines[0];
        let uris: Vec<String> = Self::decode_uris(uris_str)?;

        // Parse remaining lines
        let mut gid = 0u64;
        let mut paused = false;
        let mut options = HashMap::new();
        let mut total_length = 0u64;
        let mut completed_length = 0u64;
        let mut upload_length = 0u64;
        let mut status = "active".to_string();
        let mut error_code = 0i32;
        let mut bitfield = String::new();
        let mut num_pieces = 0u32;
        let mut piece_length = 0u32;
        let mut info_hash_hex = String::new();
        let mut resume_offset = 0u64;

        for line in &lines[1..] {
            let line = line.trim();

            // Skip empty lines
            if line.is_empty() {
                continue;
            }

            // Parse key=value pairs
            if let Some(eq_pos) = line.find('=') {
                let key = &line[..eq_pos];
                let value = &line[eq_pos + 1..];

                match key {
                    "GID" => {
                        gid = u64::from_str_radix(value, 16)
                            .unwrap_or(0);
                    }
                    "PAUSE" => {
                        paused = value == "true";
                    }
                    "TOTAL_LENGTH" => {
                        total_length = value.parse().unwrap_or(0);
                    }
                    "COMPLETED_LENGTH" => {
                        completed_length = value.parse().unwrap_or(0);
                    }
                    "UPLOAD_LENGTH" => {
                        upload_length = value.parse().unwrap_or(0);
                    }
                    "STATUS" => {
                        status = value.to_string();
                    }
                    "ERROR_CODE" => {
                        error_code = value.parse().unwrap_or(0);
                    }
                    "BITFIELD" => {
                        bitfield = value.to_string();
                    }
                    "NUM_PIECES" => {
                        num_pieces = value.parse().unwrap_or(0);
                    }
                    "PIECE_LENGTH" => {
                        piece_length = value.parse().unwrap_or(0);
                    }
                    "INFO_HASH" => {
                        info_hash_hex = value.to_string();
                    }
                    "RESUME_OFFSET" => {
                        resume_offset = value.parse().unwrap_or(0);
                    }
                    _ => {
                        // Treat as custom option
                        options.insert(key.to_string(), value.to_string());
                    }
                }
            } else {
                // Line without '=' is treated as option with empty value
                // (unusual but handle gracefully)
                options.insert(line.to_string(), String::new());
            }
        }

        Ok(SessionEntry {
            gid,
            uris,
            options,
            paused,
            total_length,
            completed_length,
            upload_length,
            download_speed: 0.0,
            upload_speed: 0.0,
            status,
            error_code,
            bitfield,
            num_pieces,
            piece_length,
            info_hash_hex,
            resume_offset,
        })
    }

    /// Encode a GID value to hex string format
    ///
    /// # Arguments
    /// * `gid` - The numeric GID
    ///
    /// # Returns
    /// * Hex-formatted string (e.g., "0000000000001234")
    pub fn encode_gid(gid: u64) -> String {
        format!("{:016x}", gid)
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

    /// Encode options HashMap to string format
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

    /// Decode options from lines
    ///
    /// Parses key=value pairs from text lines.
    ///
    /// # Arguments
    /// * `lines` - Iterator over text lines
    ///
    /// # Returns
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

    /// Encode URI list to tab-separated string
    ///
    /// # Arguments
    /// * `uris` - List of URI strings
    ///
    /// # Returns
    /// * Tab-separated URI string
    pub fn encode_uris(uris: &[String]) -> String {
        uris.join("\t")
    }

    /// Decode URIs from tab-separated string
    ///
    /// # Arguments
    /// * `uris_str` - Tab-separated URI string
    ///
    /// # Returns
    /// * Vector of individual URI strings
    pub fn decode_uris(uris_str: &str) -> Result<Vec<String>> {
        if uris_str.is_empty() {
            return Ok(Vec::new());
        }

        let uris: Vec<String> = uris_str
            .split('\t')
            .filter(|s| !s.is_empty())
            .map(|s| s.trim().to_string())
            .collect();

        if uris.is_empty() {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config("No valid URIs found".to_string()),
            ));
        }

        Ok(uris)
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
    /// * `Err(String)` describing the validation error
    pub fn validate_encoded(text: &str) -> Result<()> {
        let lines: Vec<&str> = text.lines().collect();

        if lines.is_empty() {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config("Empty entry".to_string()),
            ));
        }

        // First line must contain at least one URI
        if lines[0].trim().is_empty() {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config(
                    "First line must contain URIs".to_string(),
                ),
            ));
        }

        // Second line should be GID
        if lines.len() > 1 && !lines[1].starts_with("GID=") {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::Config(
                    "Second line must be GID".to_string(),
                ),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_basic_entry() {
        let entry = SessionEntry::new(
            0x12345678,
            vec!["http://example.com/file.zip".to_string()],
        );
        let encoded = SessionCodec::encode(&entry);

        assert!(encoded.contains("http://example.com/file.zip"));
        assert!(encoded.contains("GID=0000000012345678"));
    }

    #[test]
    fn test_encode_paused_entry() {
        let mut entry = SessionEntry::new(
            1,
            vec!["http://example.com/test.iso".to_string()],
        );
        entry.paused = true;
        entry.total_length = 1024 * 1024;
        entry.completed_length = 512 * 1024;

        let encoded = SessionCodec::encode(&entry);

        assert!(encoded.contains("PAUSE=true"));
        assert!(encoded.contains("TOTAL_LENGTH=1048576"));
        assert!(encoded.contains("COMPLETED_LENGTH=524288"));
    }

    #[test]
    fn test_decode_single_uri() {
        let text = "http://example.com/file.zip\nGID=0000000000000001";
        let entry = SessionCodec::decode(text).unwrap();

        assert_eq!(entry.uris.len(), 1);
        assert_eq!(entry.uris[0], "http://example.com/file.zip");
        assert_eq!(entry.gid, 1);
    }

    #[test]
    fn test_decode_multiple_uris() {
        let text = "http://mirror1.com/f.zip\thttp://mirror2.com/f.zip\nGID=00000000000000ABCD";
        let entry = SessionCodec::decode(text).unwrap();

        assert_eq!(entry.uris.len(), 2);
        assert_eq!(entry.gid, 0xABCD);
    }

    #[test]
    fn test_decode_with_options() {
        let text = r#"http://example.com/f.zip
GID=0000000000001234
split=4
dir=/downloads
PAUSE=true"#;

        let entry = SessionCodec::decode(text).unwrap();

        assert!(entry.paused);
        assert_eq!(entry.options.get("split").unwrap(), "4");
        assert_eq!(entry.options.get("dir").unwrap(), "/downloads");
    }

    #[test]
    fn test_decode_bt_fields() {
        let text = r#"http://example.com/torrent.torrent
GID=000000000000DEAD
BITFIELD=ff
NUM_PIECES=100
PIECE_LENGTH=262144
INFO_HASH=abcdef1234567890"#;

        let entry = SessionCodec::decode(text).unwrap();

        assert_eq!(entry.bitfield, "ff");
        assert_eq!(entry.num_pieces, 100);
        assert_eq!(entry.piece_length, 262144);
        assert_eq!(entry.info_hash_hex, "abcdef1234567890");
    }

    #[test]
    fn test_encode_gid_format() {
        let gid = SessionCodec::encode_gid(0xABCDEF01);
        assert_eq!(gid, "00000000abcdef01");
    }

    #[test]
    fn test_decode_gid_valid() {
        let gid = SessionCodec::decode_gid("0000000012345678").unwrap();
        assert_eq!(gid, 0x12345678);
    }

    #[test]
    fn test_decode_gid_with_prefix() {
        let gid = SessionCodec::decode_gid("0xABCDEF").unwrap();
        assert_eq!(gid, 0xABCDEF);
    }

    #[test]
    fn test_encode_uris_tab_separated() {
        let uris = vec![
            "http://a.com/f.zip".to_string(),
            "http://b.com/f.zip".to_string(),
            "http://c.com/f.zip".to_string(),
        ];
        let encoded = SessionCodec::encode_uris(&uris);
        assert_eq!(encoded, "http://a.com/f.zip\thttp://b.com/f.zip\thttp://c.com/f.zip");
    }

    #[test]
    fn test_validate_encoded_valid() {
        let text = "http://example.com/f.zip\nGID=0000000000000001";
        assert!(SessionCodec::validate_encoded(text).is_ok());
    }

    #[test]
    fn test_validate_encoded_no_uri() {
        let text = "\nGID=0000000000000001";
        assert!(SessionCodec::validate_encoded(text).is_err());
    }

    #[test]
    fn test_roundtrip_encode_decode() {
        let original = SessionEntry::new(
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
}
