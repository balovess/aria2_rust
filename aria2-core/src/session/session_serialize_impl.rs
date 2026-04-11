//! SessionEntry serialization/deserialization implementation
//!
//! This module provides the concrete implementation of [`SessionEntry::serialize()`]
//! and [`SessionEntry::deserialize_line()`] methods.
//!
//! # Architecture
//!
//! ```text
//! session_serialize_impl.rs (this file)
//!   ├── impl SessionEntry { serialize(), deserialize_line() }
//!   └── delegates to session_uri_utils for URI/hex handling
//!
//! session_entry.rs (core data)
//!   ├── SessionEntry struct definition
//!   └── Builder pattern methods (new, with_options, paused)
//! ```
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

use crate::error::Result;
use crate::session::session_entry::SessionEntry;
use crate::session::session_uri_utils::{decode_hex, escape_uri, unescape_uri};

impl SessionEntry {
    /// Serializes this entry to the session file format
    ///
    /// Produces a multi-line string representation suitable for writing
    /// to a session file. The format is compatible with aria2's session format.
    ///
    /// # Returns
    ///
    /// String containing the serialized entry (including trailing newline)
    ///
    /// # Format
    ///
    /// ```text
    /// uri1\turi2\turi3
    ///  GID=hex_value
    ///  [PAUSE=true]
    ///  option_key=option_value
    ///  TOTAL_LENGTH=...
    ///  COMPLETED_LENGTH=...
    ///  ...
    /// ```
    ///
    /// # Example
    ///
    /// ```rust
    /// use aria2_core::session::session_entry::SessionEntry;
    ///
    /// let entry = SessionEntry::new(0xd270c8a2, vec!["http://example.com/file.zip".to_string()]);
    /// let text = entry.serialize();
    /// assert!(text.contains("http://example.com/file.zip"));
    /// assert!(text.contains("GID=d270c8a2"));
    /// ```
    pub fn serialize(&self) -> String {
        let mut lines = String::new();

        // Serialize URIs (tab-separated, escaped)
        let escaped_uris: Vec<String> = self.uris.iter().map(|u| escape_uri(u)).collect();
        lines.push_str(&escaped_uris.join("\t"));
        lines.push('\n');

        // GID (always present, hex format)
        lines.push_str(&format!(" GID={:x}\n", self.gid));

        // PAUSE flag (only if true)
        if self.paused {
            lines.push_str(" PAUSE=true\n");
        }

        // User-defined options
        for (key, value) in &self.options {
            lines.push_str(&format!(" {}={}\n", key, value));
        }

        // Progress & status fields
        lines.push_str(&format!(" TOTAL_LENGTH={}\n", self.total_length));
        lines.push_str(&format!(" COMPLETED_LENGTH={}\n", self.completed_length));
        lines.push_str(&format!(" UPLOAD_LENGTH={}\n", self.upload_length));
        lines.push_str(&format!(" DOWNLOAD_SPEED={}\n", self.download_speed));
        lines.push_str(&format!(" STATUS={}\n", self.status));

        // ERROR_CODE (optional field)
        match self.error_code {
            Some(code) => lines.push_str(&format!(" ERROR_CODE={}\n", code)),
            None => lines.push_str(" ERROR_CODE=\n"),
        }

        // BITFIELD (hex encoded or empty)
        match &self.bitfield {
            Some(bytes) => {
                let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
                lines.push_str(&format!(" BITFIELD={}\n", hex));
            }
            None => lines.push_str(" BITFIELD=\n"),
        }

        // NUM_PIECES and PIECE_LENGTH (BT-specific)
        lines.push_str(&format!(" NUM_PIECES={}\n", self.num_pieces.unwrap_or(0)));
        lines.push_str(&format!(
            " PIECE_LENGTH={}\n",
            self.piece_length.unwrap_or(0)
        ));

        // INFO_HASH (optional, BT-specific)
        match &self.info_hash_hex {
            Some(hash) => lines.push_str(&format!(" INFO_HASH={}\n", hash)),
            None => lines.push_str(" INFO_HASH=\n"),
        }

        // RESUME_OFFSET (optional, HTTP/FTP resume support)
        lines.push_str(&format!(
            " RESUME_OFFSET={}\n",
            self.resume_offset.unwrap_or(0)
        ));

        lines
    }

    /// Deserializes a single line of text into a SessionEntry
    ///
    /// Parses a text block containing URI lines and property lines
    /// (lines starting with space) into a complete SessionEntry.
    ///
    /// # Arguments
    ///
    /// * `text` - Multi-line string containing one entry's data
    ///
    /// # Returns
    ///
    /// Result containing the deserialized SessionEntry or an error
    ///
    /// # Note
    ///
    /// This is a lower-level parsing function. For parsing multiple entries
    /// from a full session file, use [`crate::session::session_serializer::deserialize()`].
    pub fn deserialize_line(text: &str) -> Result<SessionEntry> {
        let mut entry = SessionEntry::new(0, Vec::new());

        for raw_line in text.lines() {
            let line = raw_line.trim_end();

            // Skip empty lines and comments within an entry
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Check if this is a property line (starts with space)
            if let Some(rest) = line.strip_prefix(' ') {
                let rest_trimmed = rest.trim();
                if let Some((key, value)) = rest_trimmed.split_once('=') {
                    let key = key.to_string();
                    let value = value.to_string();

                    // Handle known keys
                    match key.as_str() {
                        "GID" => {
                            if let Ok(gid) = u64::from_str_radix(&value, 16) {
                                entry.gid = gid;
                            }
                        }
                        "PAUSE" => {
                            if value == "true" {
                                entry.paused = true;
                            }
                        }
                        // Progress & status fields
                        "TOTAL_LENGTH" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.total_length = v;
                            }
                        }
                        "COMPLETED_LENGTH" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.completed_length = v;
                            }
                        }
                        "UPLOAD_LENGTH" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.upload_length = v;
                            }
                        }
                        "DOWNLOAD_SPEED" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.download_speed = v;
                            }
                        }
                        "STATUS" => {
                            if !value.is_empty() {
                                entry.status = value;
                            }
                        }
                        "ERROR_CODE" => {
                            if !value.is_empty() {
                                if let Ok(code) = value.parse::<i32>() {
                                    entry.error_code = Some(code);
                                }
                            } else {
                                entry.error_code = None;
                            }
                        }
                        "BITFIELD" => {
                            if !value.is_empty() {
                                // Decode hex string back to Vec<u8>
                                if let Ok(bytes) = decode_hex(&value) {
                                    entry.bitfield = Some(bytes);
                                } else {
                                    tracing::warn!("Invalid BITFIELD hex string, ignoring");
                                    entry.bitfield = None;
                                }
                            } else {
                                entry.bitfield = None;
                            }
                        }
                        "NUM_PIECES" => {
                            if let Ok(v) = value.parse::<u32>() {
                                if v > 0 {
                                    entry.num_pieces = Some(v);
                                } else {
                                    entry.num_pieces = None;
                                }
                            }
                        }
                        "PIECE_LENGTH" => {
                            if let Ok(v) = value.parse::<u32>() {
                                if v > 0 {
                                    entry.piece_length = Some(v);
                                } else {
                                    entry.piece_length = None;
                                }
                            }
                        }
                        "INFO_HASH" => {
                            if !value.is_empty() {
                                entry.info_hash_hex = Some(value);
                            } else {
                                entry.info_hash_hex = None;
                            }
                        }
                        "RESUME_OFFSET" => {
                            if let Ok(v) = value.parse::<u64>() {
                                if v > 0 {
                                    entry.resume_offset = Some(v);
                                } else {
                                    entry.resume_offset = None;
                                }
                            }
                        }
                        _ => {
                            // Unknown key - store in options map (forward compatibility)
                            tracing::debug!("Unknown session key '{}', storing in options", key);
                            entry.options.insert(key, value);
                        }
                    }
                }
                continue;
            }

            // This must be the URI line (first line without leading space)
            let unescaped = unescape_uri(line.trim());
            let uris: Vec<String> = unescaped
                .split('\t')
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !uris.is_empty() {
                entry.uris = uris;
            }
        }

        Ok(entry)
    }
}
