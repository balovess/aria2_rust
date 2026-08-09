//! FileEntry struct definition, construction, simple accessors, and comparison traits.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use super::types::UriResult;
use crate::download::request::Request;

// ---------------------------------------------------------------------------
// FileEntry — per-file tracking object
// ---------------------------------------------------------------------------

/// Per-file tracking object within a multi-source/multi-file download.
///
/// Each `FileEntry` manages the URI lifecycle, request pool, and in-flight
/// requests for one file. The 3-tier URI state machine is:
///
/// ```text
/// remaining_uris (not yet used) → spent_uris (dispatched) → uri_results (finished)
/// ```
///
/// The 3-tier Request state machine is:
///
/// ```text
/// request_pool (idle, sorted by speed) → in_flight_requests (active) → discarded
/// ```
///
/// # Ordering
///
/// `FileEntry` implements `Ord`/`PartialOrd` by `offset`, matching the C++
/// `operator<` semantics.
#[derive(Debug)]
pub struct FileEntry {
    // ── File metadata ────────────────────────────────────────────────────
    /// Length of this file entry in bytes.
    pub(super) length: u64,
    /// Global byte offset within the multi-file container.
    pub(super) offset: u64,

    // ── URI state machine ────────────────────────────────────────────────
    /// URIs not yet used or currently in-flight.
    pub(super) remaining_uris: VecDeque<String>,
    /// URIs already dispatched (consumed from `remaining_uris`).
    pub(super) spent_uris: VecDeque<String>,
    /// URI attempt results, sorted ascending by time of result.
    pub(super) uri_results: VecDeque<UriResult>,

    // ── Request state machine ────────────────────────────────────────────
    /// Idle/queued requests sorted by avg download speed (fastest first).
    pub(super) request_pool: Vec<Arc<Request>>,
    /// Currently active requests.
    pub(super) in_flight_requests: Vec<Arc<Request>>,

    // ── File paths ───────────────────────────────────────────────────────
    /// Local file path for saving.
    pub(super) path: String,
    /// Content-Type header value.
    pub(super) content_type: String,
    /// Original filename before rename.
    pub(super) original_name: String,
    /// `path` without parent directory; used for PREF_DIR option.
    pub(super) suffix_path: String,

    // ── Timing / connection control ──────────────────────────────────────
    /// Timestamp of last faster-server replacement.
    pub(super) last_faster_replace: Instant,
    /// Max concurrent connections to the same host.
    pub(super) max_connection_per_server: usize,

    // ── Flags ────────────────────────────────────────────────────────────
    /// Whether this file is selected for download.
    pub(super) requested: bool,
    /// All URIs use the same protocol.
    pub(super) unique_protocol: bool,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl Default for FileEntry {
    fn default() -> Self {
        Self {
            length: 0,
            offset: 0,
            remaining_uris: VecDeque::new(),
            spent_uris: VecDeque::new(),
            uri_results: VecDeque::new(),
            request_pool: Vec::new(),
            in_flight_requests: Vec::new(),
            path: String::new(),
            content_type: String::new(),
            original_name: String::new(),
            suffix_path: String::new(),
            last_faster_replace: Instant::now(),
            max_connection_per_server: 1,
            requested: false,
            unique_protocol: false,
        }
    }
}

impl FileEntry {
    /// Create a new `FileEntry` with the given path, length, offset, and URIs.
    ///
    /// Sets `requested` to `true` (matching C++ parameterized constructor).
    /// URIs are validated — only parseable URIs are kept.
    pub fn new(path: String, length: u64, offset: u64, uris: Vec<String>) -> Self {
        let mut entry = Self {
            length,
            offset,
            path,
            requested: true,
            ..Self::default()
        };
        // Add URIs via add_uri to validate each one.
        for uri in uris {
            entry.add_uri(&uri);
        }
        entry
    }

    // =====================================================================
    // Path / Name accessors
    // =====================================================================

    /// Return the local file path for saving.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Set the local file path.
    pub fn set_path(&mut self, path: String) {
        self.path = path;
    }

    /// Return the basename (filename) portion of `path`.
    ///
    /// Returns an empty string if `path` is empty.
    pub fn basename(&self) -> String {
        if self.path.is_empty() {
            return String::new();
        }
        Path::new(&self.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Return the directory portion of `path`.
    ///
    /// Returns an empty string if `path` is empty.
    pub fn dirname(&self) -> String {
        if self.path.is_empty() {
            return String::new();
        }
        Path::new(&self.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Return the original filename (before rename).
    pub fn original_name(&self) -> &str {
        &self.original_name
    }

    /// Set the original filename.
    pub fn set_original_name(&mut self, name: String) {
        self.original_name = name;
    }

    /// Return the suffix path (path without parent directory).
    pub fn suffix_path(&self) -> &str {
        &self.suffix_path
    }

    /// Set the suffix path.
    pub fn set_suffix_path(&mut self, suffix_path: String) {
        self.suffix_path = suffix_path;
    }

    /// Return the Content-Type header value.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Set the Content-Type header value.
    pub fn set_content_type(&mut self, content_type: String) {
        self.content_type = content_type;
    }

    // =====================================================================
    // Length / Offset
    // =====================================================================

    /// Return the file length in bytes.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Set the file length.
    pub fn set_length(&mut self, length: u64) {
        self.length = length;
    }

    /// Return the global byte offset within the multi-file container.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Set the global byte offset.
    pub fn set_offset(&mut self, offset: u64) {
        self.offset = offset;
    }

    /// Return `offset + length`, the first byte past this file's range.
    pub fn last_offset(&self) -> u64 {
        self.offset.saturating_add(self.length)
    }

    /// Translate a global offset to a file-local offset.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `goff < offset`.
    pub fn gtoloff(&self, goff: u64) -> u64 {
        debug_assert!(
            self.offset <= goff,
            "gtoloff: global offset {} < file offset {}",
            goff,
            self.offset
        );
        goff.saturating_sub(self.offset)
    }

    // =====================================================================
    // Requested / UniqueProtocol flags
    // =====================================================================

    /// Return whether this file is selected for download.
    pub fn is_requested(&self) -> bool {
        self.requested
    }

    /// Set the requested flag.
    pub fn set_requested(&mut self, flag: bool) {
        self.requested = flag;
    }

    /// Return whether all URIs use the same protocol.
    pub fn is_unique_protocol(&self) -> bool {
        self.unique_protocol
    }

    /// Set the unique-protocol flag.
    pub fn set_unique_protocol(&mut self, flag: bool) {
        self.unique_protocol = flag;
    }

    // =====================================================================
    // Connection control
    // =====================================================================

    /// Return the max concurrent connections per server.
    pub fn max_connection_per_server(&self) -> usize {
        self.max_connection_per_server
    }

    /// Set the max concurrent connections per server.
    pub fn set_max_connection_per_server(&mut self, n: usize) {
        self.max_connection_per_server = n.max(1);
    }

    // =====================================================================
    // Runtime resource management
    // =====================================================================

    /// Release all runtime resources (pooled and in-flight requests).
    pub fn release_runtime_resource(&mut self) {
        self.request_pool.clear();
        self.in_flight_requests.clear();
    }

    /// Check if the local file exists on disk.
    pub fn exists(&self) -> bool {
        !self.path.is_empty() && Path::new(&self.path).exists()
    }

    // =====================================================================
    // Comparison
    // =====================================================================

    /// Compare by offset (for sorting file entries by position).
    pub fn cmp_by_offset(&self, other: &FileEntry) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }
}

// ---------------------------------------------------------------------------
// PartialOrd / Ord by offset
// ---------------------------------------------------------------------------

impl PartialEq for FileEntry {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
    }
}

impl Eq for FileEntry {}

impl Ord for FileEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for FileEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
