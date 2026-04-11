//! Session Entry module - Core data structure for session serialization
//!
//! This module provides the `SessionEntry` struct which represents a single download
//! task's state that can be serialized to and deserialized from session files.
//!
//! # Overview
//!
//! A `SessionEntry` captures all necessary information about an active or paused download:
//! - **URIs**: One or more source URLs (mirrors) for the download
//! - **GID**: Unique identifier for the download task
//! - **Options**: Download configuration options (split, dir, out, etc.)
//! - **Progress**: Current download/upload statistics
//! - **Status**: Active state of the download (active, waiting, paused, error)
//! - **BT-specific**: Bitfield and piece information for BitTorrent downloads
//!
//! # Architecture
//!
//! ```text
//! session_entry.rs (this file)
//!   ├── SessionEntry struct definition
//!   ├── Builder pattern methods (new, with_options, paused)
//!   └── Re-exports for backward compatibility
//!
//! session_serialize_impl.rs
//!   └── impl SessionEntry { serialize(), deserialize_line() }
//!
//! session_uri_utils.rs
//!   └── escape_uri(), unescape_uri(), decode_hex()
//!
//! session_options.rs
//!   └── download_options_to_map()
//! ```
//!
//! # Examples
//!
//! ```rust
//! use aria2_core::session::session_entry::SessionEntry;
//! use std::collections::HashMap;
//!
//! // Create a basic entry
//! let entry = SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
//!
//! // Add options using builder pattern
//! let entry = SessionEntry::new(2, vec!["http://example.com/big.iso".to_string()])
//!     .with_options({
//!         let mut opts = HashMap::new();
//!         opts.insert("split".to_string(), "4".to_string());
//!         opts.insert("dir".to_string(), "/downloads".to_string());
//!         opts
//!     })
//!     .paused();
//! ```

// Re-exports for backward compatibility (API unchanged for external users)
pub use crate::session::session_options::download_options_to_map;
pub use crate::session::session_uri_utils::{decode_hex, escape_uri, unescape_uri};

use std::collections::HashMap;

/// Represents a single download task in a session file
///
/// This struct contains all information needed to resume a download task,
/// including URIs, options, current progress, and status.
///
/// # Fields
///
/// * `gid` - Unique global identifier for this download task
/// * `uris` - List of source URLs (primary URL + mirrors)
/// * `options` - Download configuration options as key-value pairs
/// * `paused` - Whether this download is currently paused
/// * `total_length` - Total size of the download in bytes
/// * `completed_length` - Number of bytes already downloaded
/// * `upload_length` - Number of bytes uploaded (for seeding)
/// * `download_speed` - Current download speed in bytes/sec
/// * `status` - Current status: "active", "waiting", "paused", or "error"
/// * `error_code` - Error code if status is "error"
/// * `bitfield` - BitTorrent piece completion bitmap (BT only)
/// * `num_pieces` - Number of pieces in torrent (BT only)
/// * `piece_length` - Size of each piece in bytes (BT only)
/// * `info_hash_hex` - Torrent info hash hex string (BT only)
/// * `resume_offset` - File offset for HTTP/FTP resume support
#[derive(Debug, Clone)]
pub struct SessionEntry {
    /// Unique global identifier for this download task
    pub gid: u64,

    /// List of source URIs (primary URL + mirrors), tab-separated in serialized form
    pub uris: Vec<String>,

    /// Download configuration options as key-value pairs
    pub options: HashMap<String, String>,

    /// Whether this download is currently paused
    pub paused: bool,

    // ==================== Progress & Status Fields ====================
    /// Total size of the download in bytes (0 if unknown)
    pub total_length: u64,

    /// Number of bytes already downloaded and verified
    pub completed_length: u64,

    /// Number of bytes uploaded (relevant for BitTorrent seeding)
    pub upload_length: u64,

    /// Current download speed in bytes/second
    pub download_speed: u64,

    /// Current status of the download: "active", "waiting", "paused", "error"
    pub status: String,

    /// Error code if the download is in error state
    pub error_code: Option<i32>,

    // ==================== BitTorrent-Specific Fields ====================
    // These fields are only populated for BitTorrent downloads
    /// Completed piece bitmap encoded as hex string in file format
    /// None for non-BT downloads
    pub bitfield: Option<Vec<u8>>,

    /// Total number of pieces in the torrent
    /// None for non-BT downloads
    pub num_pieces: Option<u32>,

    /// Size of each piece in bytes
    /// None for non-BT downloads
    pub piece_length: Option<u32>,

    /// Info hash of the torrent (hex string) for matching torrent files
    /// None for non-BT downloads
    pub info_hash_hex: Option<String>,

    // ==================== HTTP/FTP Resume Support ====================
    /// File offset where download should resume (for HTTP/FTP range requests)
    /// None if resumption is not applicable
    pub resume_offset: Option<u64>,
}

impl SessionEntry {
    /// Creates a new SessionEntry with default values
    ///
    /// # Arguments
    ///
    /// * `gid` - Unique identifier for this download task
    /// * `uris` - List of source URLs (at least one required)
    ///
    /// # Returns
    ///
    /// A new `SessionEntry` instance with sensible defaults:
    /// - `paused`: false
    /// - All progress fields: 0
    /// - `status`: "active"
    /// - All optional fields: None
    ///
    /// # Example
    ///
    /// ```rust
    /// use aria2_core::session::session_entry::SessionEntry;
    ///
    /// let entry = SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
    /// assert_eq!(entry.gid, 1);
    /// assert_eq!(entry.uris.len(), 1);
    /// assert!(!entry.paused);
    /// assert_eq!(entry.status, "active");
    /// ```
    pub fn new(gid: u64, uris: Vec<String>) -> Self {
        SessionEntry {
            gid,
            uris,
            options: HashMap::new(),
            paused: false,

            // Default values for progress fields
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            status: "active".to_string(),
            error_code: None,

            // BT-specific fields (None for non-BT downloads by default)
            bitfield: None,
            num_pieces: None,
            piece_length: None,
            info_hash_hex: None,

            // HTTP/FTP resume info (None by default)
            resume_offset: None,
        }
    }

    /// Sets download options using builder pattern
    ///
    /// # Arguments
    ///
    /// * `options` - HashMap of option key-value pairs
    ///
    /// # Returns
    ///
    /// Self for method chaining
    ///
    /// # Example
    ///
    /// ```rust
    /// use aria2_core::session::session_entry::SessionEntry;
    /// use std::collections::HashMap;
    ///
    /// let mut opts = HashMap::new();
    /// opts.insert("split".to_string(), "4".to_string());
    /// opts.insert("dir".to_string(), "/downloads".to_string());
    ///
    /// let entry = SessionEntry::new(1, vec!["http://example.com/f".to_string()])
    ///     .with_options(opts);
    /// assert_eq!(entry.options.get("split").unwrap(), "4");
    /// ```
    pub fn with_options(mut self, options: HashMap<String, String>) -> Self {
        self.options = options;
        self
    }

    /// Marks this entry as paused using builder pattern
    ///
    /// # Returns
    ///
    /// Self for method chaining
    ///
    /// # Example
    ///
    /// ```rust
    /// use aria2_core::session::session_entry::SessionEntry;
    ///
    /// let entry = SessionEntry::new(1, vec!["http://example.com/f".to_string()])
    ///     .paused();
    /// assert!(entry.paused);
    /// ```
    pub fn paused(mut self) -> Self {
        self.paused = true;
        self
    }

    /// Gets an option value by key
    ///
    /// # Arguments
    ///
    /// * `key` - Option key to look up
    ///
    /// # Returns
    ///
    /// Some(&str) if the key exists, None otherwise
    #[allow(dead_code)] // Utility method for option retrieval, available for future use
    fn get_option(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(|s| s.as_str())
    }

    // Note: serialize() and deserialize_line() are now implemented in
    // session_serialize_impl.rs as part of impl SessionEntry
    // They are available via the impl block there and accessible normally
}

// ==================== Unit Tests ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_single_entry() {
        let entry = SessionEntry::new(0xd270c8a2, vec!["http://example.com/file.zip".to_string()]);
        let text = entry.serialize();
        assert!(
            text.contains("http://example.com/file.zip"),
            "Should contain URI"
        );
        assert!(text.contains("GID=d270c8a2"), "Should contain GID");
    }

    #[test]
    fn test_serialize_multiple_entries_roundtrip() {
        let entries = vec![
            SessionEntry::new(1, vec!["http://a.com/1.bin".to_string()]).with_options({
                let mut m = HashMap::new();
                m.insert("split".to_string(), "4".to_string());
                m.insert("dir".to_string(), "/tmp".to_string());
                m
            }),
            SessionEntry::new(
                2,
                vec![
                    "ftp://b.com/2.iso".to_string(),
                    "http://mirror.b.com/2.iso".to_string(),
                ],
            )
            .paused(),
        ];

        let mut serialized = String::new();
        for e in &entries {
            serialized.push_str(&e.serialize());
            serialized.push('\n');
        }

        // Parse individually using deserialize_line
        let parts: Vec<&str> = serialized.split("\n\n").collect();
        assert!(parts.len() >= 2, "Should have at least 2 entries");

        let entry1 = SessionEntry::deserialize_line(parts[0]).unwrap();
        assert_eq!(entry1.uris.len(), 1);
        assert_eq!(entry1.uris[0], "http://a.com/1.bin");
        assert_eq!(entry1.options.get("split").unwrap(), "4");

        let entry2 = SessionEntry::deserialize_line(parts[1]).unwrap();
        assert_eq!(entry2.uris.len(), 2);
        assert!(entry2.paused);
    }

    #[test]
    fn test_deserialize_empty_file() {
        let entry = SessionEntry::deserialize_line("").unwrap();
        assert!(entry.uris.is_empty());

        let entry = SessionEntry::deserialize_line("\n\n\n").unwrap();
        assert!(entry.uris.is_empty());
    }

    #[test]
    fn test_deserialize_skip_comments_and_blanks() {
        let input = r#"# This is a comment
# Another comment

http://example.com/file
 GID=abc123
 dir=/downloads
"#;
        let entry = SessionEntry::deserialize_line(input).unwrap();
        // Should parse first entry and ignore comments
        assert_eq!(entry.uris.len(), 1);
        assert_eq!(entry.uris[0], "http://example.com/file");
    }

    #[test]
    fn test_deserialize_options_parsing() {
        let input = r#"http://example.com/file.zip
 GID=1
 split=4
 max-connection-per-server=2
 dir=C:\Users\test\Downloads
 out=file.zip
"#;
        let entry = SessionEntry::deserialize_line(input).unwrap();
        assert_eq!(entry.options.get("split").unwrap(), "4");
        assert_eq!(entry.options.get("max-connection-per-server").unwrap(), "2");
        assert_eq!(
            entry.options.get("dir").unwrap(),
            "C:\\Users\\test\\Downloads"
        );
        assert_eq!(entry.options.get("out").unwrap(), "file.zip");
    }

    #[test]
    fn test_pause_flag_serialization() {
        let input = r#"http://example.com/pause.me
 GID=42
 PAUSE=true
"#;
        let entry = SessionEntry::deserialize_line(input).unwrap();
        assert!(entry.paused);

        let text = entry.serialize();
        assert!(text.contains("PAUSE=true"));
    }

    #[test]
    fn test_serialize_tab_separated_uris() {
        let entry = SessionEntry::new(
            99,
            vec![
                "http://mirror1.com/f".to_string(),
                "http://mirror2.com/f".to_string(),
                "http://mirror3.com/f".to_string(),
            ],
        );
        let text = entry.serialize();
        let uri_line = text.lines().next().unwrap();
        assert_eq!(
            uri_line.matches('\t').count(),
            2,
            "3 URIs should have 2 tab separators"
        );
    }

    // ==================== New Field Tests (Session Persistence Enhancement) ====================

    #[test]
    fn test_serialize_new_fields() {
        let mut entry = SessionEntry::new(1, vec!["http://example.com/file.bin".to_string()]);
        entry.total_length = 1024 * 1024; // 1MB
        entry.completed_length = 512 * 1024; // 512KB
        entry.upload_length = 1024;
        entry.download_speed = 2048;
        entry.status = "active".to_string();
        entry.error_code = None;

        let text = entry.serialize();

        // Verify new fields appear in output
        assert!(
            text.contains("TOTAL_LENGTH=1048576"),
            "Should contain TOTAL_LENGTH"
        );
        assert!(
            text.contains("COMPLETED_LENGTH=524288"),
            "Should contain COMPLETED_LENGTH"
        );
        assert!(
            text.contains("UPLOAD_LENGTH=1024"),
            "Should contain UPLOAD_LENGTH"
        );
        assert!(
            text.contains("DOWNLOAD_SPEED=2048"),
            "Should contain DOWNLOAD_SPEED"
        );
        assert!(text.contains("STATUS=active"), "Should contain STATUS");
    }

    #[test]
    fn test_deserialize_with_all_fields() {
        let input = r#"http://example.com/bigfile.zip
 GID=1
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=5242880
 UPLOAD_LENGTH=2048
 DOWNLOAD_SPEED=4096
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=5242880
"#;

        let entry = SessionEntry::deserialize_line(input).unwrap();

        assert_eq!(entry.total_length, 10485760);
        assert_eq!(entry.completed_length, 5242880);
        assert_eq!(entry.upload_length, 2048);
        assert_eq!(entry.download_speed, 4096);
        assert_eq!(entry.status, "active");
        assert_eq!(entry.error_code, None);
        assert_eq!(entry.resume_offset, Some(5242880));
    }

    #[test]
    fn test_deserialize_backward_compat() {
        // Old format (without new fields) should load correctly
        let input = r#"http://example.com/old-format.zip
 GID=abc123
 split=4
 dir=/downloads
"#;

        let entry = SessionEntry::deserialize_line(input).unwrap();

        // Verify defaults
        assert_eq!(entry.total_length, 0, "Old format should use default 0");
        assert_eq!(entry.completed_length, 0, "Old format should use default 0");
        assert_eq!(entry.upload_length, 0, "Old format should use default 0");
        assert_eq!(entry.download_speed, 0, "Old format should use default 0");
        assert_eq!(
            entry.status, "active",
            "Old format should default to 'active'"
        );
        assert_eq!(
            entry.error_code, None,
            "Old format should have no error code"
        );
        assert_eq!(entry.bitfield, None, "Old format should have no bitfield");
        assert_eq!(
            entry.resume_offset, None,
            "Old format should have no resume_offset"
        );

        // Original fields still correct
        assert_eq!(entry.options.get("split").unwrap(), "4");
    }

    #[test]
    fn test_deserialize_unknown_keys_ignored() {
        // Input with unknown keys should not cause parse failure (forward compatibility)
        let input = r#"http://example.com/file.zip
 GID=1
 UNKNOWN_KEY=some_value
 ANOTHER_UNKNOWN=42
 FUTURE_FIELD=data
 TOTAL_LENGTH=1000
"#;

        let entry = SessionEntry::deserialize_line(input).unwrap();

        // Known fields parsed normally
        assert_eq!(entry.total_length, 1000);

        // Unknown keys stored in options
        assert_eq!(entry.options.get("UNKNOWN_KEY").unwrap(), "some_value");
        assert_eq!(entry.options.get("ANOTHER_UNKNOWN").unwrap(), "42");
        assert_eq!(entry.options.get("FUTURE_FIELD").unwrap(), "data");
    }

    #[test]
    fn test_bitfield_roundtrip() {
        let mut entry =
            SessionEntry::new(1, vec!["http://example.com/torrent.torrent".to_string()]);

        // Set bitfield: [0xFF, 0xF0, 0x0F] - indicates some pieces completed
        entry.bitfield = Some(vec![0xFF, 0xF0, 0x0F]);
        entry.num_pieces = Some(24); // 3 bytes * 8 bits = 24 pieces
        entry.piece_length = Some(262144); // 256KB

        let text = entry.serialize();

        // Verify hex encoding
        assert!(
            text.contains("BITFIELD=fff00f"),
            "bitfield should be encoded as hex string"
        );

        // Deserialize verification
        let restored = SessionEntry::deserialize_line(&text).unwrap();
        assert_eq!(
            restored.bitfield,
            Some(vec![0xFF, 0xF0, 0x0F]),
            "bitfield should be restored correctly"
        );
        assert_eq!(restored.num_pieces, Some(24));
        assert_eq!(restored.piece_length, Some(262144));
    }

    #[test]
    fn test_empty_bitfield_serialized_as_empty() {
        let entry = SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
        // bitfield defaults to None

        let text = entry.serialize();

        // None bitfield should produce empty value
        assert!(
            text.contains("BITFIELD=\n"),
            "None bitfield should be serialized as empty value"
        );

        // Deserialize verification
        let restored = SessionEntry::deserialize_line(&text).unwrap();
        assert_eq!(
            restored.bitfield, None,
            "Empty bitfield should restore to None"
        );
    }

    #[test]
    fn test_default_session_entry_has_zero_progress() {
        let entry = SessionEntry::new(99, vec!["http://test.com/f".to_string()]);

        // Verify all new fields have correct defaults
        assert_eq!(entry.total_length, 0);
        assert_eq!(entry.completed_length, 0);
        assert_eq!(entry.upload_length, 0);
        assert_eq!(entry.download_speed, 0);
        assert_eq!(entry.status, "active", "Default status should be 'active'");
        assert_eq!(entry.error_code, None);
        assert_eq!(entry.bitfield, None);
        assert_eq!(entry.num_pieces, None);
        assert_eq!(entry.piece_length, None);
        assert_eq!(entry.info_hash_hex, None);
        assert_eq!(entry.resume_offset, None);
    }

    #[test]
    fn test_status_field_values() {
        let statuses = ["active", "waiting", "paused", "error"];

        for status in statuses {
            let mut entry = SessionEntry::new(1, vec!["http://example.com/f".to_string()]);
            entry.status = status.to_string();

            let text = entry.serialize();
            assert!(
                text.contains(&format!("STATUS={}", status)),
                "Status '{}' should be serialized correctly",
                status
            );

            // Deserialize verification
            let restored = SessionEntry::deserialize_line(&text).unwrap();
            assert_eq!(
                restored.status, status,
                "Status '{}' should be deserialized correctly",
                status
            );
        }
    }

    #[test]
    fn test_resume_offset_for_http_ftp() {
        let mut entry = SessionEntry::new(1, vec!["http://example.com/large-file.iso".to_string()]);

        // Simulate HTTP download with partial data written
        entry.total_length = 1073741824; // 1GB
        entry.completed_length = 536870912; // 512MB completed
        entry.resume_offset = Some(536870912); // Resume from 512MB
        entry.status = "paused".to_string();

        let text = entry.serialize();

        // Verify resume offset is serialized correctly
        assert!(
            text.contains("RESUME_OFFSET=536870912"),
            "resume offset should be serialized correctly"
        );

        // Deserialize and verify
        let restored = SessionEntry::deserialize_line(&text).unwrap();
        assert_eq!(
            restored.resume_offset,
            Some(536870912),
            "resume offset should be restored correctly"
        );
        assert_eq!(restored.status, "paused");
    }

    #[test]
    fn test_bt_specific_fields_only_when_present() {
        // Test that BT-specific fields are truly optional
        let mut entry = SessionEntry::new(1, vec!["magnet:?xt=urn:btih:abc123".to_string()]);

        // Don't set any BT fields (keep them as None)
        let text_without_bt = entry.serialize();
        let restored_without_bt = SessionEntry::deserialize_line(&text_without_bt).unwrap();

        assert_eq!(restored_without_bt.bitfield, None);
        assert_eq!(restored_without_bt.num_pieces, None);
        assert_eq!(restored_without_bt.piece_length, None);
        assert_eq!(restored_without_bt.info_hash_hex, None);

        // Now set BT fields
        entry.bitfield = Some(vec![0xAA, 0xBB]);
        entry.num_pieces = Some(16);
        entry.piece_length = Some(524288);
        entry.info_hash_hex = Some("abc123def456".to_string());

        let text_with_bt = entry.serialize();
        let restored_with_bt = SessionEntry::deserialize_line(&text_with_bt).unwrap();

        assert_eq!(restored_with_bt.bitfield, Some(vec![0xAA, 0xBB]));
        assert_eq!(restored_with_bt.num_pieces, Some(16));
        assert_eq!(restored_with_bt.piece_length, Some(524288));
        assert_eq!(
            restored_with_bt.info_hash_hex,
            Some("abc123def456".to_string())
        );
    }
}
