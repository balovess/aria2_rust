//! SessionEntry struct definition and builder methods
//!
//! Core data structure representing a single download task's state that can
//! be serialized to and deserialized from session files.

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
    /// ```no_run
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
    fn get_opt(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(|s| s.as_str())
    }

    // Note: serialize() and deserialize_line() are implemented in
    // session_serialize_impl.rs as part of impl SessionEntry.
    // They are available via the impl block there and accessible normally.
}