//! Session Entry module - Core data structure for session serialization
//!
//! This module provides the `SessionEntry` struct which represents a single download
//! task's state that can be serialized to and deserialized from session files.
//! It includes URI handling, option storage, progress tracking, and BT-specific fields.
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

use std::collections::HashMap;

use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadOptions;

/// Represents a single download task in a session file
///
/// This struct contains all information needed to resume a download task,
/// including URIs, options, current progress, and status.
///
/// # Fields
///
/// * `gid` - Unique global identifier for this download task
/// * `uris` - List of source URLs (primary + mirrors)
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
    fn get_option(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(|s| s.as_str())
    }

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
        lines.push_str(&format!(
            " NUM_PIECES={}\n",
            self.num_pieces.unwrap_or(0)
        ));
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

// ==================== Helper Functions ====================

/// Escapes special characters in URIs for safe serialization
///
/// Replaces special characters with escape sequences:
/// - `\` → `\\`
/// - `\t` (tab) → `\t`
/// - `\n` (newline) → `\n`
///
/// # Arguments
///
/// * `s` - Input string to escape
///
/// # Returns
///
/// Escaped string safe for inclusion in session file format
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_entry::{escape_uri, unescape_uri};
///
/// let escaped = escape_uri("path\\to\tfile\nname");
/// assert_eq!(escaped, "path\\\\to\\tfile\\nname");
/// assert_eq!(unescape_uri(&escaped), "path\\to\tfile\nname");
/// ```
pub fn escape_uri(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

/// Unescapes special characters previously escaped by [`escape_uri()`]
///
/// Processes escape sequences:
/// - `\\` → `\`
/// - `\t` → tab character
/// - `\n` → newline character
///
/// # Arguments
///
/// * `s` - Escaped input string
///
/// # Returns
///
/// Unescaped original string
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_entry::{escape_uri, unescape_uri};
///
/// assert_eq!(unescape_uri(&escape_uri("hello\tworld")), "hello\tworld");
/// assert_eq!(unescape_uri(&escape_uri("line1\nline2")), "line1\nline2");
/// assert_eq!(unescape_uri(&escape_uri("back\\slash")), "back\\slash");
/// ```
pub fn unescape_uri(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                match next {
                    't' => {
                        result.push('\t');
                        chars.next();
                    }
                    'n' => {
                        result.push('\n');
                        chars.next();
                    }
                    '\\' => {
                        result.push('\\');
                        chars.next();
                    }
                    _ => {
                        result.push(c);
                    }
                }
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Decodes a hexadecimal string to a byte vector
///
/// Converts a string of hex characters (e.g., "ff00ff") into
/// the corresponding bytes ([0xFF, 0x00, xFF]).
///
/// # Arguments
///
/// * `hex` - Hexadecimal string (must have even length)
///
/// # Returns
///
/// Result containing the decoded byte vector or an error if:
/// - The hex string has odd length
/// - Contains invalid hex characters
///
/// # Errors
///
/// Returns [`Aria2Error::Io`] if the hex string is malformed
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_entry::decode_hex;
///
/// let bytes = decode_hex("fff00f").unwrap();
/// assert_eq!(bytes, vec![0xFF, 0xF0, 0x0F]);
/// ```
pub fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err(Aria2Error::Io(format!(
            "Hex string has odd length: {}",
            hex.len()
        )));
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);

    for i in (0..hex.len()).step_by(2) {
        let byte_str = &hex[i..i + 2];
        let byte = u8::from_str_radix(byte_str, 16).map_err(|e| {
            Aria2Error::Io(format!("Invalid hex character at position {}: {}", i, e))
        })?;
        bytes.push(byte);
    }

    Ok(bytes)
}

// ==================== DownloadOptions Conversion ====================

/// Converts DownloadOptions struct to a HashMap for serialization
///
/// Extracts relevant fields from [`DownloadOptions`] and converts them
/// to key-value pairs suitable for session file format.
///
/// # Arguments
///
/// * `opts` - DownloadOptions struct from RequestGroup
///
/// # Returns
///
/// HashMap containing option key-value pairs
///
/// # Mapped Fields
///
/// | DownloadOptions Field | Session Key |
/// |---------------------|-------------|
/// | split | "split" |
/// | max_connection_per_server | "max-connection-per-server" |
/// | max_download_limit | "max-download-limit" |
/// | max_upload_limit | "max-upload-limit" |
/// | dir | "dir" |
/// | out | "out" |
/// | seed_time | "seed-time" |
/// | seed_ratio | "seed-ratio" |
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_entry::download_options_to_map;
/// use aria2_core::request::request_group::DownloadOptions;
///
/// let opts = DownloadOptions {
///     split: Some(8),
///     max_connection_per_server: Some(4),
///     ..Default::default()
/// };
///
/// let map = download_options_to_map(&opts);
/// assert_eq!(map.get("split").unwrap(), "8");
/// assert_eq!(map.get("max-connection-per-server").unwrap(), "4");
/// ```
pub fn download_options_to_map(opts: &DownloadOptions) -> HashMap<String, String> {
    let mut map = HashMap::new();

    // Connection settings
    if let Some(v) = opts.split {
        map.insert("split".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_connection_per_server {
        map.insert("max-connection-per-server".to_string(), v.to_string());
    }

    // Bandwidth limits
    if let Some(v) = opts.max_download_limit {
        map.insert("max-download-limit".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_upload_limit {
        map.insert("max-upload-limit".to_string(), v.to_string());
    }

    // Output settings
    if let Some(ref v) = opts.dir {
        map.insert("dir".to_string(), v.clone());
    }
    if let Some(ref v) = opts.out {
        map.insert("out".to_string(), v.clone());
    }

    // Seeding settings (BitTorrent)
    if let Some(v) = opts.seed_time {
        map.insert("seed-time".to_string(), v.to_string());
    }
    if let Some(v) = opts.seed_ratio {
        map.insert("seed-ratio".to_string(), v.to_string());
    }

    map
}

// ==================== Unit Tests ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_single_entry() {
        let entry =
            SessionEntry::new(0xd270c8a2, vec!["http://example.com/file.zip".to_string()]);
        let text = entry.serialize();
        assert!(text.contains("http://example.com/file.zip"), "Should contain URI");
        assert!(text.contains("GID=d270c8a2"), "Should contain GID");
    }

    #[test]
    fn test_serialize_multiple_entries_roundtrip() {
        let entries = vec![
            SessionEntry::new(
                1,
                vec!["http://a.com/1.bin".to_string()],
            )
            .with_options({
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
    fn test_escape_unescape_uri() {
        assert_eq!(unescape_uri(&escape_uri("hello\tworld")), "hello\tworld");
        assert_eq!(unescape_uri(&escape_uri("line1\nline2")), "line1\nline2");
        assert_eq!(unescape_uri(&escape_uri("back\\slash")), "back\\slash");
        assert_eq!(unescape_uri(&escape_uri("normal")), "normal");
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
        assert_eq!(
            entry.options.get("max-connection-per-server").unwrap(),
            "2"
        );
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

    #[tokio::test]
    async fn test_download_options_to_map_coverage() {
        let opts = DownloadOptions {
            split: Some(8),
            max_connection_per_server: Some(4),
            max_download_limit: Some(102400),
            max_upload_limit: Some(51200),
            dir: Some("/data".to_string()),
            out: Some("output.bin".to_string()),
            seed_time: Some(300),
            seed_ratio: Some(2.0),
            checksum: None,
            cookie_file: None,
            cookies: None,
            bt_force_encrypt: false,
            bt_require_crypto: false,
            enable_dht: true,
            dht_listen_port: Some(6881),
            enable_public_trackers: true,
            bt_piece_selection_strategy: "rarest-first".to_string(),
            bt_endgame_threshold: 20,
            max_retries: 3,
            retry_wait: 1,
            http_proxy: None,
            dht_file_path: None,
            // Choking algorithm configuration
            bt_max_upload_slots: Some(4),
            bt_optimistic_unchoke_interval: Some(30),
            bt_snubbed_timeout: Some(60),
        };

        let map = download_options_to_map(&opts);
        assert_eq!(map.get("split").unwrap(), "8");
        assert_eq!(map.get("seed-ratio").unwrap(), "2");

        let empty_opts = DownloadOptions::default();
        let empty_map = download_options_to_map(&empty_opts);
        assert!(empty_map.is_empty());
    }

    // ==================== New Field Tests (Session Persistence Enhancement) ====================

    #[test]
    fn test_serialize_new_fields() {
        let mut entry =
            SessionEntry::new(1, vec!["http://example.com/file.bin".to_string()]);
        entry.total_length = 1024 * 1024; // 1MB
        entry.completed_length = 512 * 1024; // 512KB
        entry.upload_length = 1024;
        entry.download_speed = 2048;
        entry.status = "active".to_string();
        entry.error_code = None;

        let text = entry.serialize();

        // Verify new fields appear in output
        assert!(text.contains("TOTAL_LENGTH=1048576"), "Should contain TOTAL_LENGTH");
        assert!(
            text.contains("COMPLETED_LENGTH=524288"),
            "Should contain COMPLETED_LENGTH"
        );
        assert!(text.contains("UPLOAD_LENGTH=1024"), "Should contain UPLOAD_LENGTH");
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
        assert_eq!(entry.status, "active", "Old format should default to 'active'");
        assert_eq!(entry.error_code, None, "Old format should have no error code");
        assert_eq!(entry.bitfield, None, "Old format should have no bitfield");
        assert_eq!(entry.resume_offset, None, "Old format should have no resume_offset");

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
        let mut entry = SessionEntry::new(
            1,
            vec!["http://example.com/torrent.torrent".to_string()],
        );

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
        let entry =
            SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
        // bitfield defaults to None

        let text = entry.serialize();

        // None bitfield should produce empty value
        assert!(
            text.contains("BITFIELD=\n"),
            "None bitfield should be serialized as empty value"
        );

        // Deserialize verification
        let restored = SessionEntry::deserialize_line(&text).unwrap();
        assert_eq!(restored.bitfield, None, "Empty bitfield should restore to None");
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
            let mut entry =
                SessionEntry::new(1, vec!["http://example.com/f".to_string()]);
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
        let mut entry = SessionEntry::new(
            1,
            vec!["http://example.com/large-file.iso".to_string()],
        );

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
        let mut entry =
            SessionEntry::new(1, vec!["magnet:?xt=urn:btih:abc123".to_string()]);

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

    #[test]
    fn test_decode_hex_valid() {
        // Test valid hex strings
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode_hex("00").unwrap(), vec![0x00]);
        assert_eq!(decode_hex("ff").unwrap(), vec![0xFF]);
        assert_eq!(decode_hex("fff00f").unwrap(), vec![0xFF, 0xF0, 0x0F]);
        assert_eq!(
            decode_hex("deadbeef").unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn test_decode_hex_invalid() {
        // Test invalid hex strings
        assert!(decode_hex("abc").is_err()); // Odd length
        assert!(decode_hex("ghij").is_err()); // Invalid characters
    }
}
