//! Download context — central metadata binding file entries, URIs, and download metadata.
//!
//! Equivalent to the C++ aria2 `DownloadContext` class. This is the primary
//! data object associated with a single download task, holding:
//!
//! - **File entries** — ordered list of files (single for HTTP, multi for torrent/metalink)
//! - **Piece hashes** — per-piece hash values for verification
//! - **Whole-file checksum** — digest and algorithm for full-file verification
//! - **Network stats** — per-download speed / byte counters
//! - **Attributes** — typed extension map (BitTorrent, Ed2k, etc.)
//! - **Signature** — optional Metalink/PGP signature
//!
//! # Design differences from C++ aria2
//!
//! | C++ aria2 | Rust | Rationale |
//! |---|---|---|
//! | `RequestGroup*` raw pointer | `owner_request_group_id: Option<u64>` | No raw pointers; ID-based reference |
//! | `vector<shared_ptr<ContextAttribute>>` fixed-size | `HashMap<ContextAttributeType, Box<dyn Any + Send + Sync>>` | More flexible, Rust-idiomatic; thread-safe |
//! | `Timer` / `wallclock` | `Instant` | Standard library monotonic clock |
//! | `A2STR::NIL` for missing piece hash | Returns `""` via static | Same semantics, zero-allocation |

use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::{debug, trace};

use super::file_entry::FileEntry;

// ---------------------------------------------------------------------------
// ContextAttributeType
// ---------------------------------------------------------------------------

/// Typed keys for the attribute extension map on `DownloadContext`.
///
/// Mirrors the C++ `ContextAttributeType` enum. The `Ed2k` variant is an
/// aria2-next addition; `BitTorrent` is the original attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextAttributeType {
    BitTorrent,
    Ed2k,
}

// ---------------------------------------------------------------------------
// TorrentAttribute — BitTorrent-specific download metadata
// ---------------------------------------------------------------------------

/// BitTorrent file mode — single vs multi-file torrent.
///
/// Mirrors C++ `BtFileMode` enum. Used in `TorrentAttribute::mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtFileMode {
    /// Single-file torrent (one file in the info dict).
    Single,
    /// Multi-file torrent (directory with multiple files in the info dict).
    Multi,
}

impl Default for BtFileMode {
    fn default() -> Self {
        BtFileMode::Single
    }
}

/// BitTorrent-specific attributes stored on `DownloadContext`.
///
/// Mirrors C++ `bittorrent::TorrentAttribute` which is accessed via
/// `bittorrent::getTorrentAttrs(DownloadContext*)`. In C++ this is a struct
/// inheriting from `ContextAttribute` with the following fields:
/// `name`, `mode`, `announceList`, `nodes`, `infoHash`, `metadata`,
/// `metadataSize`, `privateTorrent`, `creationDate`, `comment`,
/// `createdBy`, `urlList`.
///
/// All fields from C++ are present here. The Rust version uses owned types
/// instead of C++ raw pointers/strings.
#[derive(Debug, Clone)]
pub struct TorrentAttribute {
    /// Torrent name from the info dict.
    /// C++ `name` — e.g. "debian-13.5.0-amd64-DVD-1"
    pub name: String,

    /// File mode (single vs multi).
    /// C++ `mode` — `BtFileMode::SINGLE` or `BtFileMode::MULTI`
    pub mode: BtFileMode,

    /// Announce URL list from the .torrent file or magnet URI.
    /// C++ `announceList` — tiered list of tracker URLs.
    pub announce_list: Vec<Vec<String>>,

    /// DHT bootstrap nodes from the .torrent file.
    /// C++ `nodes` — `vector<pair<string, uint16_t>>` for DHT bootstrap.
    pub nodes: Vec<(String, u16)>,

    /// 20-byte info hash in hexadecimal (40 chars).
    /// C++ `infoHash` — identifies the torrent for tracker/DHT/PEX.
    pub info_hash: String,

    /// Raw torrent metadata (bencoded info dict bytes).
    /// C++ `metadata` — used for ut_metadata extension (BEP 9).
    /// Empty for regular torrents (metadata already available), populated
    /// for magnet links after metadata exchange completes.
    pub metadata: Vec<u8>,

    /// Size of the metadata in bytes (for ut_metadata extension).
    /// C++ `metadataSize` — 0 when metadata is already available.
    pub metadata_size: usize,

    /// Whether this is a private torrent (BEP 0027).
    /// C++ `privateTorrent` — when true, DHT/PEX/LPD must be disabled.
    pub private_torrent: bool,

    /// Creation date from the .torrent file (Unix timestamp).
    /// C++ `creationDate` — 0 when not present in the torrent.
    pub creation_date: i64,

    /// Comment from the .torrent file.
    /// C++ `comment` — empty when not present.
    pub comment: String,

    /// Creator field from the .torrent file.
    /// C++ `createdBy` — empty when not present.
    pub created_by: String,

    /// Web seed URLs from the .torrent url-list field.
    /// C++ `urlList` — HTTP/FTP seeds for hybrid downloading.
    pub url_list: Vec<String>,
}

impl TorrentAttribute {
    /// Create a new `TorrentAttribute` with the given info hash.
    ///
    /// All other fields default to empty/zero values. This is the minimal
    /// constructor used when only the info hash is known (e.g. magnet link
    /// before metadata exchange).
    pub fn new(info_hash: String) -> Self {
        Self {
            name: String::new(),
            mode: BtFileMode::Single,
            announce_list: Vec::new(),
            nodes: Vec::new(),
            info_hash,
            metadata: Vec::new(),
            metadata_size: 0,
            private_torrent: false,
            creation_date: 0,
            comment: String::new(),
            created_by: String::new(),
            url_list: Vec::new(),
        }
    }

    /// Create a `TorrentAttribute` from a 20-byte raw info hash.
    pub fn from_bytes(info_hash_bytes: &[u8; 20]) -> Self {
        Self::new(hex::encode(info_hash_bytes))
    }

    /// Whether the metadata has been received (for magnet links).
    ///
    /// In C++, this is checked via `metadata.size() > 0`. We use
    /// `metadata_size > 0 || !metadata.is_empty()` which is equivalent.
    pub fn metadata_received(&self) -> bool {
        self.metadata_size > 0 || !self.metadata.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Signature
// ---------------------------------------------------------------------------

/// Placeholder for Metalink / PGP signature data.
///
/// Will be expanded with actual PGP parsing when Metalink support is
/// fully wired in.
#[derive(Debug, Clone)]
pub struct Signature {
    /// Raw signature body (ASCII-armored or binary)
    pub body: String,
    /// Hash algorithm used for the signature (e.g. "sha-1", "sha-256")
    pub hash_type: String,
}

impl Signature {
    /// Create a new signature with the given body and hash type.
    pub fn new(body: String, hash_type: String) -> Self {
        Self { body, hash_type }
    }
}

// ---------------------------------------------------------------------------
// NetStat
// ---------------------------------------------------------------------------

/// Per-download network statistics.
///
/// Tracks download/upload byte counters and speed. The speed fields are
/// updated externally (e.g. by the download engine's rolling-window
/// calculator); the counters are incremented via [`DownloadContext::update_download`]
/// and [`DownloadContext::update_upload_length`].
#[derive(Debug)]
pub struct NetStat {
    /// Cumulative bytes downloaded in the current session.
    session_download_length: u64,
    /// Cumulative bytes uploaded in the current session.
    session_upload_length: u64,
    /// Current download speed (bytes/sec), updated externally.
    download_speed: u64,
    /// Current upload speed (bytes/sec), updated externally.
    upload_speed: u64,
    /// Monotonic timestamp when the download started.
    download_start_time: Option<Instant>,
    /// Monotonic timestamp when the download stopped.
    download_stop_time: Option<Instant>,
}

impl Default for NetStat {
    fn default() -> Self {
        Self {
            session_download_length: 0,
            session_upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            download_start_time: None,
            download_stop_time: None,
        }
    }
}

impl NetStat {
    /// Mark the download as started — records the current time.
    pub fn download_start(&mut self) {
        self.download_start_time = Some(Instant::now());
    }

    /// Mark the download as stopped — records the current time.
    pub fn download_stop(&mut self) {
        self.download_stop_time = Some(Instant::now());
    }

    /// Add `bytes` to the session download counter.
    pub fn update_download(&mut self, bytes: u64) {
        self.session_download_length += bytes;
    }

    /// Add `bytes` to the session upload counter.
    pub fn update_upload_length(&mut self, bytes: u64) {
        self.session_upload_length += bytes;
    }

    /// Set the upload speed (bytes/sec).
    pub fn update_upload_speed(&mut self, bytes: u64) {
        self.upload_speed = bytes;
    }

    /// Return the session download length.
    pub fn session_download_length(&self) -> u64 {
        self.session_download_length
    }

    /// Return the session upload length.
    pub fn session_upload_length(&self) -> u64 {
        self.session_upload_length
    }

    /// Return the current download speed.
    pub fn download_speed(&self) -> u64 {
        self.download_speed
    }

    /// Set the current download speed.
    pub fn set_download_speed(&mut self, speed: u64) {
        self.download_speed = speed;
    }

    /// Return the current upload speed.
    pub fn upload_speed(&self) -> u64 {
        self.upload_speed
    }

    /// Return the recorded download start time.
    pub fn download_start_time(&self) -> Option<Instant> {
        self.download_start_time
    }

    /// Return the recorded download stop time.
    pub fn download_stop_time(&self) -> Option<Instant> {
        self.download_stop_time
    }

    /// Calculate the session duration.
    ///
    /// Returns the elapsed time between `download_start_time` and
    /// `download_stop_time`. If either is missing, returns `Duration::ZERO`.
    pub fn calculate_session_time(&self) -> Duration {
        match (self.download_start_time, self.download_stop_time) {
            (Some(start), Some(stop)) => stop.duration_since(start),
            _ => Duration::ZERO,
        }
    }
}

// ---------------------------------------------------------------------------
// DownloadContext
// ---------------------------------------------------------------------------

/// Central metadata object binding file entries, URIs, and download metadata.
///
/// Each `RequestGroup` owns one `DownloadContext`. The context holds the
/// ordered list of [`FileEntry`]s, per-piece and whole-file hash information,
/// network statistics, and a typed attribute map for protocol-specific data
/// (BitTorrent metadata, Ed2k info, etc.).
///
/// # Thread safety
///
/// `DownloadContext` is **not** `Sync` — it is designed to be owned by a
/// single `RequestGroup` and accessed through that group's lock. If shared
/// access is needed, wrap it in `Arc<Mutex<_>>` or `Arc<RwLock<_>>`.
///
/// # Example
///
/// ```
/// use aria2_core::download::download_context::DownloadContext;
///
/// // Create a single-file download context
/// let ctx = DownloadContext::new(1048576, 1024 * 1024 * 100, "/tmp/file.bin".into());
/// assert_eq!(ctx.get_piece_length(), 1048576);
/// assert_eq!(ctx.get_total_length(), 104857600);
/// assert!(ctx.knows_total_length());
/// ```
pub struct DownloadContext {
    // -- Optional signature (Metalink/PGP) --
    signature: Option<Signature>,

    // -- Back-pointer to owning RequestGroup (ID-based, not a raw pointer) --
    owner_request_group_id: Option<u64>,

    // -- Typed attribute extension map --
    attrs: HashMap<ContextAttributeType, Box<dyn Any + Send + Sync>>,

    // -- Ordered list of files in this download --
    file_entries: Vec<FileEntry>,

    // -- Per-piece hash values for verification --
    piece_hashes: Vec<String>,

    // -- Per-download network statistics --
    net_stat: NetStat,

    // -- Timestamp when download stopped --
    download_stop_time: Option<Instant>,

    // -- Hash algorithm name for piece hashes --
    piece_hash_type: String,

    // -- Whole-file hash digest value --
    digest: String,

    // -- Whole-file hash algorithm name --
    hash_type: String,

    // -- Override path for .aria2 control file naming --
    base_path: String,

    // -- Piece length in bytes (0 = unknown) --
    piece_length: u32,

    // -- Whether the whole-file checksum has already been verified --
    checksum_verified: bool,

    // -- Whether total length is known --
    knows_total_length: bool,

    // -- Whether to parse Metalink info from response headers --
    accept_metalink: bool,
}

impl std::fmt::Debug for DownloadContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadContext")
            .field("owner_request_group_id", &self.owner_request_group_id)
            .field("attrs_count", &self.attrs.len())
            .field("file_entries", &self.file_entries.len())
            .field("piece_hashes_count", &self.piece_hashes.len())
            .field("net_stat", &self.net_stat)
            .field("piece_hash_type", &self.piece_hash_type)
            .field("base_path", &self.base_path)
            .field("piece_length", &self.piece_length)
            .field("checksum_verified", &self.checksum_verified)
            .field("knows_total_length", &self.knows_total_length)
            .field("accept_metalink", &self.accept_metalink)
            .finish()
    }
}

// Static empty string for returning references to "no hash" without allocation.
static EMPTY_STRING: &str = "";

impl DownloadContext {
    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    /// Create a `DownloadContext` with default values.
    ///
    /// - `piece_length` = 0 (unknown)
    /// - `knows_total_length` = true
    /// - `checksum_verified` = false
    /// - `accept_metalink` = true (matches C++ `ENABLE_METALINK` default)
    /// - No file entries, no hashes, no signature.
    pub fn new_default() -> Self {
        Self {
            signature: None,
            owner_request_group_id: None,
            attrs: HashMap::new(),
            file_entries: Vec::new(),
            piece_hashes: Vec::new(),
            net_stat: NetStat::default(),
            download_stop_time: None,
            piece_hash_type: String::new(),
            digest: String::new(),
            hash_type: String::new(),
            base_path: String::new(),
            piece_length: 0,
            checksum_verified: false,
            knows_total_length: true,
            accept_metalink: true,
        }
    }

    /// Create a `DownloadContext` for a single-file download.
    ///
    /// Convenience constructor that creates one `FileEntry` with the given
    /// `path`, `total_length`, and offset 0. The `piece_length` is stored
    /// directly.
    ///
    /// # Arguments
    ///
    /// * `piece_length` - Piece length in bytes (0 = unknown)
    /// * `total_length` - Total file size in bytes
    /// * `path` - File path (should be pre-escaped if needed)
    pub fn new(piece_length: u32, total_length: u64, path: String) -> Self {
        let file_entry = FileEntry::new(path, total_length, 0, Vec::new());
        Self {
            piece_length,
            file_entries: vec![file_entry],
            signature: None,
            owner_request_group_id: None,
            attrs: HashMap::new(),
            piece_hashes: Vec::new(),
            net_stat: NetStat::default(),
            download_stop_time: None,
            piece_hash_type: String::new(),
            digest: String::new(),
            hash_type: String::new(),
            base_path: String::new(),
            checksum_verified: false,
            knows_total_length: true,
            accept_metalink: true,
        }
    }

    // -----------------------------------------------------------------------
    // Total Length / Knowledge
    // -----------------------------------------------------------------------

    /// Derive the total length from file entries.
    ///
    /// Returns `file_entries.last().last_offset()`, or 0 if empty.
    /// This matches the C++ implementation where total length is not stored
    /// independently but computed from the last file entry's offset + length.
    pub fn get_total_length(&self) -> u64 {
        self.file_entries
            .last()
            .map(|fe| fe.last_offset())
            .unwrap_or(0)
    }

    /// Whether the total download length is known.
    pub fn knows_total_length(&self) -> bool {
        self.knows_total_length
    }

    /// Mark the total length as unknown (e.g. content-length missing).
    pub fn mark_total_length_is_unknown(&mut self) {
        self.knows_total_length = false;
        debug!("Total length marked as unknown");
    }

    /// Mark the total length as known.
    pub fn mark_total_length_is_known(&mut self) {
        self.knows_total_length = true;
        debug!("Total length marked as known");
    }

    // -----------------------------------------------------------------------
    // File Entries
    // -----------------------------------------------------------------------

    /// Return a reference to the ordered file entry list.
    pub fn get_file_entries(&self) -> &[FileEntry] {
        &self.file_entries
    }

    /// Return a mutable reference to the ordered file entry list.
    pub fn get_file_entries_mut(&mut self) -> &mut Vec<FileEntry> {
        &mut self.file_entries
    }

    /// Return a reference to the first file entry.
    ///
    /// # Panics
    ///
    /// Panics if there are no file entries (matches C++ `assert`).
    pub fn get_first_file_entry(&self) -> &FileEntry {
        self.file_entries
            .first()
            .expect("get_first_file_entry: no file entries")
    }

    /// Return a reference to the first file entry whose `is_requested()` is true.
    ///
    /// Returns `None` if no such file entry exists.
    pub fn get_first_requested_file_entry(&self) -> Option<&FileEntry> {
        self.file_entries.iter().find(|fe| fe.is_requested())
    }

    /// Count the number of file entries whose `is_requested()` is true.
    pub fn count_requested_file_entry(&self) -> usize {
        self.file_entries.iter().filter(|fe| fe.is_requested()).count()
    }

    /// Replace the file entry list with a new vector.
    pub fn set_file_entries(&mut self, entries: Vec<FileEntry>) {
        self.file_entries = entries;
        trace!(count = self.file_entries.len(), "File entries replaced");
    }

    /// Find the file entry that contains the given byte offset.
    ///
    /// Uses binary search over the sorted-by-offset file entries.
    /// Returns `None` if the offset is out of range or no file entries exist.
    ///
    /// # Algorithm
    ///
    /// Matches C++ `findFileEntryByOffset`:
    /// 1. Reject if empty or offset beyond the last file's end.
    /// 2. Use `partition_point` to find the insertion point for `offset`.
    /// 3. If the entry at the insertion point starts exactly at `offset`, return it.
    /// 4. Otherwise return the preceding entry (the file containing `offset`).
    pub fn find_file_entry_by_offset(&self, offset: u64) -> Option<&FileEntry> {
        if self.file_entries.is_empty() {
            return None;
        }
        let last_offset = self.file_entries.last().unwrap().last_offset();
        if offset > 0 && last_offset <= offset {
            return None;
        }

        // partition_point: find first entry whose offset > the target offset
        let idx = self
            .file_entries
            .partition_point(|fe| fe.offset() <= offset);

        if idx > 0 {
            // The entry at idx-1 has offset <= our target.
            // If idx is in bounds and its offset == target, it's an exact match;
            // otherwise the preceding entry contains the offset.
            if idx < self.file_entries.len() && self.file_entries[idx].offset() == offset {
                Some(&self.file_entries[idx])
            } else {
                Some(&self.file_entries[idx - 1])
            }
        } else {
            // idx == 0 means offset < first entry's offset, which shouldn't
            // happen for valid offsets (offset 0 maps to the first entry).
            // But if the first entry starts at offset 0, partition_point returns 1.
            // If we reach here, the offset is before all entries.
            None
        }
    }

    // -----------------------------------------------------------------------
    // Piece Info
    // -----------------------------------------------------------------------

    /// Return the piece length in bytes (0 = unknown).
    pub fn get_piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Set the piece length in bytes.
    pub fn set_piece_length(&mut self, length: u32) {
        self.piece_length = length;
        debug!(piece_length = length, "Piece length updated");
    }

    /// Calculate the number of pieces.
    ///
    /// Returns `(last_offset + piece_length - 1) / piece_length`, or 0 if
    /// `piece_length` is 0 or there are no file entries.
    pub fn get_num_pieces(&self) -> usize {
        if self.piece_length == 0 || self.file_entries.is_empty() {
            return 0;
        }
        let last_offset = self.file_entries.last().unwrap().last_offset();
        ((last_offset + self.piece_length as u64 - 1) / self.piece_length as u64) as usize
    }

    // -----------------------------------------------------------------------
    // Piece Hash Access
    // -----------------------------------------------------------------------

    /// Return the piece hash at the given index, or an empty string if
    /// out of bounds.
    ///
    /// Matches C++ `getPieceHash` which returns `A2STR::NIL` for invalid
    /// indices. We return a static `&str` to avoid allocation.
    pub fn get_piece_hash(&self, index: usize) -> &str {
        self.piece_hashes.get(index).map(|s| s.as_str()).unwrap_or(EMPTY_STRING)
    }

    /// Return a reference to all piece hashes.
    pub fn get_piece_hashes(&self) -> &[String] {
        &self.piece_hashes
    }

    /// Return the hash algorithm used for piece hashes (e.g. "sha-1").
    pub fn get_piece_hash_type(&self) -> &str {
        &self.piece_hash_type
    }

    /// Set piece hashes and their algorithm.
    ///
    /// Replaces any existing piece hashes.
    pub fn set_piece_hashes(&mut self, hash_type: String, hashes: Vec<String>) {
        self.piece_hash_type = hash_type;
        self.piece_hashes = hashes;
        debug!(
            hash_type = %self.piece_hash_type,
            count = self.piece_hashes.len(),
            "Piece hashes set"
        );
    }

    // -----------------------------------------------------------------------
    // Whole-file Checksum
    // -----------------------------------------------------------------------

    /// Return the whole-file hash digest value.
    pub fn get_digest(&self) -> &str {
        &self.digest
    }

    /// Return the whole-file hash algorithm name.
    pub fn get_hash_type(&self) -> &str {
        &self.hash_type
    }

    /// Set the whole-file checksum.
    pub fn set_digest(&mut self, hash_type: String, digest: String) {
        self.hash_type = hash_type;
        self.digest = digest;
        debug!(hash_type = %self.hash_type, "Whole-file checksum set");
    }

    /// Whether a whole-file checksum verification is needed.
    ///
    /// Returns `true` when:
    /// - No piece hash type is set (piece-level verification won't happen), AND
    /// - Both digest and hash_type are present, AND
    /// - The checksum has NOT been verified yet.
    ///
    /// This matches the C++ `isChecksumVerificationNeeded()` logic.
    pub fn is_checksum_verification_needed(&self) -> bool {
        self.piece_hash_type.is_empty()
            && !self.digest.is_empty()
            && !self.hash_type.is_empty()
            && !self.checksum_verified
    }

    /// Whether a whole-file checksum is available (digest + hash_type present).
    pub fn is_checksum_verification_available(&self) -> bool {
        !self.digest.is_empty() && !self.hash_type.is_empty()
    }

    /// Whether piece hash verification is available.
    ///
    /// Returns `true` when:
    /// - `piece_hash_type` is non-empty, AND
    /// - At least one piece hash exists, AND
    /// - The number of piece hashes equals `get_num_pieces()`.
    pub fn is_piece_hash_verification_available(&self) -> bool {
        !self.piece_hash_type.is_empty()
            && !self.piece_hashes.is_empty()
            && self.piece_hashes.len() == self.get_num_pieces()
    }

    /// Whether a whole-file checksum verification is pending (aria2-next).
    ///
    /// Stricter than `is_checksum_verification_needed()`: returns true whenever
    /// a whole-file hash is available and NOT verified, regardless of whether
    /// piece hash verification is also available.
    pub fn is_checksum_verification_pending(&self) -> bool {
        self.is_checksum_verification_available() && !self.checksum_verified
    }

    /// Set whether the checksum has been verified.
    pub fn set_checksum_verified(&mut self, verified: bool) {
        self.checksum_verified = verified;
        debug!(verified, "Checksum verified flag updated");
    }

    // -----------------------------------------------------------------------
    // Path
    // -----------------------------------------------------------------------

    /// Return the representative path for this context.
    ///
    /// Used as part of the `.aria2` control file name. If `base_path` is set,
    /// returns `base_path`. Otherwise returns the first file entry's path.
    ///
    /// # Panics
    ///
    /// Panics if `base_path` is empty and there are no file entries.
    pub fn get_base_path(&self) -> &str {
        if !self.base_path.is_empty() {
            &self.base_path
        } else {
            self.get_first_file_entry().path()
        }
    }

    /// Set an override path for the `.aria2` control file naming.
    pub fn set_base_path(&mut self, path: String) {
        self.base_path = path;
    }

    // -----------------------------------------------------------------------
    // Signature
    // -----------------------------------------------------------------------

    /// Return a reference to the optional signature.
    pub fn get_signature(&self) -> Option<&Signature> {
        self.signature.as_ref()
    }

    /// Set the signature, replacing any existing one.
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = Some(signature);
    }

    // -----------------------------------------------------------------------
    // Owner RequestGroup
    // -----------------------------------------------------------------------

    /// Return the ID of the owning `RequestGroup`, if set.
    pub fn get_owner_request_group_id(&self) -> Option<u64> {
        self.owner_request_group_id
    }

    /// Set the ID of the owning `RequestGroup`.
    pub fn set_owner_request_group_id(&mut self, id: u64) {
        self.owner_request_group_id = Some(id);
    }

    // -----------------------------------------------------------------------
    // File Filter
    // -----------------------------------------------------------------------

    /// Mark file entries as requested / not-requested based on a sorted
    /// list of 1-based indices.
    ///
    /// If the index list is empty or there is only one file entry, all
    /// entries are marked as requested. Otherwise, entries whose 1-based
    /// index appears in `indices` are marked requested; all others are
    /// marked not-requested.
    ///
    /// # Arguments
    ///
    /// * `indices` - Sorted, deduplicated, 1-based file indices.
    ///   Must be >= 1.
    pub fn set_file_filter(&mut self, mut indices: Vec<usize>) {
        // If no filter or single-file, all entries are requested
        if indices.is_empty() || self.file_entries.len() <= 1 {
            for fe in &mut self.file_entries {
                fe.set_requested(true);
            }
            return;
        }

        // Sort and dedup for safety
        indices.sort_unstable();
        indices.dedup();

        let mut filter_iter = indices.iter().peekable();
        for (i, fe) in self.file_entries.iter_mut().enumerate() {
            // Convert to 1-based index for comparison
            let one_based = i + 1;
            match filter_iter.peek() {
                Some(&idx) if *idx == one_based => {
                    fe.set_requested(true);
                    let _ = filter_iter.next();
                }
                Some(&idx) if *idx > one_based => {
                    fe.set_requested(false);
                }
                Some(_) => {
                    // idx < one_based shouldn't happen with sorted input,
                    // but mark not-requested as fallback
                    fe.set_requested(false);
                }
                None => {
                    fe.set_requested(false);
                }
            }
        }

        debug!(
            total = self.file_entries.len(),
            requested = self.count_requested_file_entry(),
            "File filter applied"
        );
    }

    /// Set the file path for the entry at the given 1-based index.
    ///
    /// # Errors
    ///
    /// Returns an error if `index` is 0 or exceeds the number of file entries.
    pub fn set_file_path_with_index(&mut self, index: usize, path: String) -> Result<(), String> {
        if index == 0 || index > self.file_entries.len() {
            return Err(format!("No such file with index={}", index));
        }
        // Path is not escaped here — matches C++ behavior
        self.file_entries[index - 1].set_path(path);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Attributes
    // -----------------------------------------------------------------------

    /// Set a typed attribute, replacing any existing one for the same key.
    pub fn set_attribute(&mut self, key: ContextAttributeType, value: Box<dyn Any + Send + Sync>) {
        self.attrs.insert(key, value);
    }

    /// Get a reference to the attribute for the given key.
    ///
    /// Returns `None` if no attribute is set for that key.
    pub fn get_attribute(&self, key: ContextAttributeType) -> Option<&(dyn Any + Send + Sync)> {
        self.attrs.get(&key).map(|b| b.as_ref())
    }

    /// Whether an attribute is set for the given key.
    pub fn has_attribute(&self, key: ContextAttributeType) -> bool {
        self.attrs.contains_key(&key)
    }

    /// Return a reference to the full attribute map.
    pub fn get_attributes(&self) -> &HashMap<ContextAttributeType, Box<dyn Any + Send + Sync>> {
        &self.attrs
    }

    /// Get the BT info hash hex string, if a TorrentAttribute is set.
    ///
    /// Mirrors C++ `bittorrent::getTorrentAttrs(ctx)->infoHash`.
    /// Returns `None` if no BitTorrent attribute is set or it cannot
    /// be downcast to `TorrentAttribute`.
    pub fn get_bt_info_hash_hex(&self) -> Option<String> {
        self.get_attribute(ContextAttributeType::BitTorrent)
            .and_then(|attr| attr.downcast_ref::<TorrentAttribute>())
            .map(|ta| ta.info_hash.clone())
    }

    // -----------------------------------------------------------------------
    // Timing
    // -----------------------------------------------------------------------

    /// Reset the download start time and clear the stop time.
    ///
    /// Records the current instant as the start time and clears the stop
    /// time, preparing for a new download session.
    pub fn reset_download_start_time(&mut self) {
        self.download_stop_time = None;
        self.net_stat.download_start();
        trace!("Download start time reset");
    }

    /// Record the download stop time as now.
    ///
    /// Also marks the network stat as stopped.
    pub fn reset_download_stop_time(&mut self) {
        self.download_stop_time = Some(Instant::now());
        self.net_stat.download_stop();
        trace!("Download stop time recorded");
    }

    /// Return the recorded download stop time.
    pub fn get_download_stop_time(&self) -> Option<Instant> {
        self.download_stop_time
    }

    /// Calculate the session duration.
    ///
    /// Returns the difference between the download start and stop times.
    /// If either is missing, returns `Duration::ZERO`.
    pub fn calculate_session_time(&self) -> Duration {
        self.net_stat.calculate_session_time()
    }

    // -----------------------------------------------------------------------
    // Metalink
    // -----------------------------------------------------------------------

    /// Whether Metalink parsing is accepted from response headers.
    pub fn get_accept_metalink(&self) -> bool {
        self.accept_metalink
    }

    /// Set whether to accept Metalink info from response headers.
    pub fn set_accept_metalink(&mut self, accept: bool) {
        self.accept_metalink = accept;
    }

    // -----------------------------------------------------------------------
    // Network Stats
    // -----------------------------------------------------------------------

    /// Return a reference to the per-download network statistics.
    pub fn get_net_stat(&self) -> &NetStat {
        &self.net_stat
    }

    /// Return a mutable reference to the per-download network statistics.
    pub fn get_net_stat_mut(&mut self) -> &mut NetStat {
        &mut self.net_stat
    }

    /// Update the download byte counter.
    ///
    /// Increments the local `NetStat`. The C++ version also updates the
    /// global `RequestGroupMan` net stat — that will be wired in later
    /// when the back-pointer mechanism is connected.
    pub fn update_download(&mut self, bytes: u64) {
        self.net_stat.update_download(bytes);
        // TODO: wire global RequestGroupMan net stat update
    }

    /// Update the upload byte counter.
    ///
    /// Same dual-update pattern as `update_download`.
    pub fn update_upload_length(&mut self, bytes: u64) {
        self.net_stat.update_upload_length(bytes);
        // TODO: wire global RequestGroupMan net stat update
    }

    /// Update the upload speed.
    pub fn update_upload_speed(&mut self, bytes: u64) {
        self.net_stat.update_upload_speed(bytes);
        // TODO: wire global RequestGroupMan net stat update
    }

    // -----------------------------------------------------------------------
    // Resource Management
    // -----------------------------------------------------------------------

    /// Release runtime resources held by all file entries.
    ///
    /// Calls `put_back_request()` and `release_runtime_resource()` on each
    /// file entry, clearing in-memory download state while preserving the
    /// metadata needed for session persistence.
    pub fn release_runtime_resource(&mut self) {
        for fe in &mut self.file_entries {
            fe.put_back_request();
            fe.release_runtime_resource();
        }
        debug!(count = self.file_entries.len(), "Runtime resources released");
    }
}

impl Default for DownloadContext {
    fn default() -> Self {
        Self::new_default()
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    // Helper: create a FileEntry with given path, length, offset
    fn make_file_entry(path: &str, length: u64, offset: u64) -> FileEntry {
        FileEntry::new(path.to_string(), length, offset, Vec::new())
    }

    // -----------------------------------------------------------------------
    // 1. Default constructor
    // -----------------------------------------------------------------------
    #[test]
    fn test_default_constructor() {
        let ctx = DownloadContext::new_default();
        assert_eq!(ctx.get_piece_length(), 0);
        assert!(ctx.knows_total_length());
        assert!(!ctx.is_checksum_verification_needed());
        assert!(!ctx.is_checksum_verification_available());
        assert!(!ctx.is_piece_hash_verification_available());
        assert!(ctx.get_accept_metalink());
        assert!(ctx.get_file_entries().is_empty());
        assert_eq!(ctx.get_total_length(), 0);
        assert!(ctx.get_signature().is_none());
        assert!(ctx.get_owner_request_group_id().is_none());
        // get_base_path() panics on empty file entries, so we skip it here
        // and test it in the base_path tests below.
    }

    // -----------------------------------------------------------------------
    // 2. Parameterized constructor (pieceLength, totalLength, path)
    // -----------------------------------------------------------------------
    #[test]
    fn test_parameterized_constructor() {
        let ctx = DownloadContext::new(1048576, 104857600, "/tmp/file.bin".into());
        assert_eq!(ctx.get_piece_length(), 1048576);
        assert_eq!(ctx.get_total_length(), 104857600);
        assert_eq!(ctx.get_file_entries().len(), 1);
        assert_eq!(ctx.get_first_file_entry().path(), "/tmp/file.bin");
        assert!(ctx.knows_total_length());
        assert!(!ctx.is_checksum_verification_needed());
    }

    // -----------------------------------------------------------------------
    // 3. File entry management
    // -----------------------------------------------------------------------
    #[test]
    fn test_file_entry_management() {
        let mut ctx = DownloadContext::new_default();

        // Add file entries
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
            make_file_entry("file3.bin", 3000, 3000),
        ]);

        assert_eq!(ctx.get_file_entries().len(), 3);
        assert_eq!(ctx.get_first_file_entry().path(), "file1.bin");
    }

    #[test]
    fn test_get_first_requested_file_entry() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
        ]);

        // By default all are requested
        let first_req = ctx.get_first_requested_file_entry();
        assert!(first_req.is_some());
        assert_eq!(first_req.unwrap().path(), "file1.bin");

        // Mark first as not requested
        ctx.get_file_entries_mut()[0].set_requested(false);
        let first_req = ctx.get_first_requested_file_entry();
        assert!(first_req.is_some());
        assert_eq!(first_req.unwrap().path(), "file2.bin");
    }

    #[test]
    fn test_count_requested_file_entry() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
            make_file_entry("file3.bin", 3000, 3000),
        ]);

        assert_eq!(ctx.count_requested_file_entry(), 3);

        ctx.get_file_entries_mut()[1].set_requested(false);
        assert_eq!(ctx.count_requested_file_entry(), 2);
    }

    // -----------------------------------------------------------------------
    // 4. findFileEntryByOffset (binary search)
    // -----------------------------------------------------------------------
    #[test]
    fn test_find_file_entry_by_offset() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),    // [0, 1000)
            make_file_entry("file2.bin", 2000, 1000), // [1000, 3000)
            make_file_entry("file3.bin", 3000, 3000), // [3000, 6000)
        ]);

        // Offset 0 -> first file
        let fe = ctx.find_file_entry_by_offset(0).unwrap();
        assert_eq!(fe.path(), "file1.bin");

        // Offset 500 -> first file
        let fe = ctx.find_file_entry_by_offset(500).unwrap();
        assert_eq!(fe.path(), "file1.bin");

        // Offset 1000 -> second file (exact boundary)
        let fe = ctx.find_file_entry_by_offset(1000).unwrap();
        assert_eq!(fe.path(), "file2.bin");

        // Offset 2500 -> second file
        let fe = ctx.find_file_entry_by_offset(2500).unwrap();
        assert_eq!(fe.path(), "file2.bin");

        // Offset 3000 -> third file (exact boundary)
        let fe = ctx.find_file_entry_by_offset(3000).unwrap();
        assert_eq!(fe.path(), "file3.bin");

        // Offset beyond range -> None
        assert!(ctx.find_file_entry_by_offset(6000).is_none());

        // Offset way beyond -> None
        assert!(ctx.find_file_entry_by_offset(99999).is_none());
    }

    #[test]
    fn test_find_file_entry_by_offset_empty() {
        let ctx = DownloadContext::new_default();
        assert!(ctx.find_file_entry_by_offset(0).is_none());
    }

    // -----------------------------------------------------------------------
    // 5. Total length derivation from file entries
    // -----------------------------------------------------------------------
    #[test]
    fn test_total_length_derivation() {
        let mut ctx = DownloadContext::new_default();

        // Empty -> 0
        assert_eq!(ctx.get_total_length(), 0);

        // Single file
        ctx.set_file_entries(vec![make_file_entry("file.bin", 5000, 0)]);
        assert_eq!(ctx.get_total_length(), 5000);

        // Multiple files
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
        ]);
        // last_offset of last entry = 1000 + 2000 = 3000
        assert_eq!(ctx.get_total_length(), 3000);
    }

    // -----------------------------------------------------------------------
    // 6. Piece hash management
    // -----------------------------------------------------------------------
    #[test]
    fn test_piece_hash_management() {
        let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());

        assert!(ctx.get_piece_hash_type().is_empty());
        assert!(ctx.get_piece_hashes().is_empty());

        ctx.set_piece_hashes(
            "sha-1".to_string(),
            vec![
                "abc123".to_string(),
                "def456".to_string(),
                "ghi789".to_string(),
                "jkl012".to_string(),
            ],
        );

        assert_eq!(ctx.get_piece_hash_type(), "sha-1");
        assert_eq!(ctx.get_piece_hashes().len(), 4);
        assert_eq!(ctx.get_piece_hash(0), "abc123");
        assert_eq!(ctx.get_piece_hash(3), "jkl012");
    }

    #[test]
    fn test_get_piece_hash_out_of_bounds() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_piece_hashes("sha-1".to_string(), vec!["abc".to_string()]);
        assert_eq!(ctx.get_piece_hash(5), "");
    }

    // -----------------------------------------------------------------------
    // 7. getNumPieces calculation
    // -----------------------------------------------------------------------
    #[test]
    fn test_get_num_pieces() {
        // 4096 bytes, 1024 piece length -> 4 pieces
        let ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
        assert_eq!(ctx.get_num_pieces(), 4);

        // 4097 bytes, 1024 piece length -> 5 pieces
        let ctx2 = DownloadContext::new(1024, 4097, "/tmp/file.bin".into());
        assert_eq!(ctx2.get_num_pieces(), 5);
    }

    #[test]
    fn test_get_num_pieces_zero_piece_length() {
        let ctx = DownloadContext::new(0, 4096, "/tmp/file.bin".into());
        assert_eq!(ctx.get_num_pieces(), 0);
    }

    // -----------------------------------------------------------------------
    // 8. Whole-file checksum management
    // -----------------------------------------------------------------------
    #[test]
    fn test_whole_file_checksum() {
        let mut ctx = DownloadContext::new_default();

        assert!(ctx.get_digest().is_empty());
        assert!(ctx.get_hash_type().is_empty());

        ctx.set_digest("sha-256".to_string(), "abcdef1234567890".to_string());

        assert_eq!(ctx.get_hash_type(), "sha-256");
        assert_eq!(ctx.get_digest(), "abcdef1234567890");
    }

    // -----------------------------------------------------------------------
    // 9. Verification availability checks
    // -----------------------------------------------------------------------
    #[test]
    fn test_is_checksum_verification_needed() {
        let mut ctx = DownloadContext::new_default();

        // No digest/hash -> not needed
        assert!(!ctx.is_checksum_verification_needed());

        // Set digest+hash but no piece hash type -> needed
        ctx.set_digest("sha-256".to_string(), "abc".to_string());
        assert!(ctx.is_checksum_verification_needed());

        // Set piece hash type -> not needed (piece verification will handle it)
        ctx.set_piece_hashes("sha-1".to_string(), vec!["h1".to_string()]);
        assert!(!ctx.is_checksum_verification_needed());

        // Remove piece hash type, mark verified -> not needed
        let mut ctx2 = DownloadContext::new_default();
        ctx2.set_digest("sha-256".to_string(), "abc".to_string());
        ctx2.set_checksum_verified(true);
        assert!(!ctx2.is_checksum_verification_needed());
    }

    #[test]
    fn test_is_checksum_verification_available() {
        let mut ctx = DownloadContext::new_default();
        assert!(!ctx.is_checksum_verification_available());

        ctx.set_digest("sha-256".to_string(), "abc".to_string());
        assert!(ctx.is_checksum_verification_available());
    }

    #[test]
    fn test_is_checksum_verification_pending() {
        let mut ctx = DownloadContext::new_default();

        // Not available -> not pending
        assert!(!ctx.is_checksum_verification_pending());

        // Available but not verified -> pending
        ctx.set_digest("sha-256".to_string(), "abc".to_string());
        assert!(ctx.is_checksum_verification_pending());

        // Available and verified -> not pending
        ctx.set_checksum_verified(true);
        assert!(!ctx.is_checksum_verification_pending());
    }

    #[test]
    fn test_is_checksum_verification_pending_with_piece_hash() {
        let mut ctx = DownloadContext::new_default();
        // Even with piece hash set, pending still returns true if
        // whole-file hash is available and not verified
        ctx.set_piece_hashes("sha-1".to_string(), vec!["h1".to_string()]);
        ctx.set_digest("sha-256".to_string(), "abc".to_string());
        // is_checksum_verification_needed would be false (piece hash type set),
        // but is_checksum_verification_pending is true (whole hash available, not verified)
        assert!(!ctx.is_checksum_verification_needed());
        assert!(ctx.is_checksum_verification_pending());
    }

    #[test]
    fn test_is_piece_hash_verification_available() {
        let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
        assert!(!ctx.is_piece_hash_verification_available());

        // Set 3 piece hashes but need 4 -> not available
        ctx.set_piece_hashes(
            "sha-1".to_string(),
            vec!["h1".to_string(), "h2".to_string(), "h3".to_string()],
        );
        assert!(!ctx.is_piece_hash_verification_available());

        // Set 4 piece hashes matching numPieces -> available
        ctx.set_piece_hashes(
            "sha-1".to_string(),
            vec![
                "h1".to_string(),
                "h2".to_string(),
                "h3".to_string(),
                "h4".to_string(),
            ],
        );
        assert!(ctx.is_piece_hash_verification_available());
    }

    // -----------------------------------------------------------------------
    // 10. BasePath with fallback to first FileEntry
    // -----------------------------------------------------------------------
    #[test]
    fn test_base_path_fallback() {
        let ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
        // No base_path set -> falls back to first file entry's path
        assert_eq!(ctx.get_base_path(), "/tmp/file.bin");
    }

    #[test]
    fn test_base_path_override() {
        let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
        ctx.set_base_path("/opt/download/file.bin".to_string());
        assert_eq!(ctx.get_base_path(), "/opt/download/file.bin");
    }

    // -----------------------------------------------------------------------
    // 11. Piece length get/set
    // -----------------------------------------------------------------------
    #[test]
    fn test_piece_length_get_set() {
        let mut ctx = DownloadContext::new_default();
        assert_eq!(ctx.get_piece_length(), 0);

        ctx.set_piece_length(262144);
        assert_eq!(ctx.get_piece_length(), 262144);
    }

    // -----------------------------------------------------------------------
    // 12. knowsTotalLength / markTotalLengthIsKnown/Unknown
    // -----------------------------------------------------------------------
    #[test]
    fn test_knows_total_length() {
        let mut ctx = DownloadContext::new_default();
        assert!(ctx.knows_total_length());

        ctx.mark_total_length_is_unknown();
        assert!(!ctx.knows_total_length());

        ctx.mark_total_length_is_known();
        assert!(ctx.knows_total_length());
    }

    // -----------------------------------------------------------------------
    // 13. Accept metalink flag
    // -----------------------------------------------------------------------
    #[test]
    fn test_accept_metalink() {
        let mut ctx = DownloadContext::new_default();
        assert!(ctx.get_accept_metalink());

        ctx.set_accept_metalink(false);
        assert!(!ctx.get_accept_metalink());

        ctx.set_accept_metalink(true);
        assert!(ctx.get_accept_metalink());
    }

    // -----------------------------------------------------------------------
    // 14. Network stats (basic update)
    // -----------------------------------------------------------------------
    #[test]
    fn test_network_stats_update() {
        let mut ctx = DownloadContext::new_default();

        ctx.update_download(100);
        ctx.update_download(200);
        assert_eq!(ctx.get_net_stat().session_download_length(), 300);

        ctx.update_upload_length(50);
        ctx.update_upload_length(25);
        assert_eq!(ctx.get_net_stat().session_upload_length(), 75);

        ctx.update_upload_speed(1024);
        assert_eq!(ctx.get_net_stat().upload_speed(), 1024);
    }

    // -----------------------------------------------------------------------
    // 15. Release runtime resources
    // -----------------------------------------------------------------------
    #[test]
    fn test_release_runtime_resource() {
        let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
        // Should not panic
        ctx.release_runtime_resource();
    }

    // -----------------------------------------------------------------------
    // 16. File filter (setFileFilter with index list)
    // -----------------------------------------------------------------------
    #[test]
    fn test_file_filter_empty_indices() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
        ]);

        // Empty filter -> all requested
        ctx.set_file_filter(vec![]);
        assert_eq!(ctx.count_requested_file_entry(), 2);
    }

    #[test]
    fn test_file_filter_single_file() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![make_file_entry("file1.bin", 1000, 0)]);

        // Single file -> all requested regardless of filter
        ctx.set_file_filter(vec![5, 10]);
        assert_eq!(ctx.count_requested_file_entry(), 1);
    }

    #[test]
    fn test_file_filter_selective() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
            make_file_entry("file3.bin", 3000, 3000),
        ]);

        // Select only file 2 (1-based index)
        ctx.set_file_filter(vec![2]);
        assert!(!ctx.get_file_entries()[0].is_requested());
        assert!(ctx.get_file_entries()[1].is_requested());
        assert!(!ctx.get_file_entries()[2].is_requested());
    }

    #[test]
    fn test_file_filter_multiple_indices() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
            make_file_entry("file3.bin", 3000, 3000),
        ]);

        // Select files 1 and 3
        ctx.set_file_filter(vec![1, 3]);
        assert!(ctx.get_file_entries()[0].is_requested());
        assert!(!ctx.get_file_entries()[1].is_requested());
        assert!(ctx.get_file_entries()[2].is_requested());
    }

    // -----------------------------------------------------------------------
    // 17. setFilePathWithIndex
    // -----------------------------------------------------------------------
    #[test]
    fn test_set_file_path_with_index() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
        ]);

        assert!(ctx.set_file_path_with_index(1, "/new/path1.bin".into()).is_ok());
        assert_eq!(ctx.get_file_entries()[0].path(), "/new/path1.bin");

        assert!(ctx.set_file_path_with_index(2, "/new/path2.bin".into()).is_ok());
        assert_eq!(ctx.get_file_entries()[1].path(), "/new/path2.bin");
    }

    #[test]
    fn test_set_file_path_with_index_out_of_bounds() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_file_entries(vec![make_file_entry("file1.bin", 1000, 0)]);

        // Index 0 is invalid
        assert!(ctx.set_file_path_with_index(0, "path".into()).is_err());

        // Index beyond length
        assert!(ctx.set_file_path_with_index(5, "path".into()).is_err());
    }

    // -----------------------------------------------------------------------
    // 18. Checksum verified flag
    // -----------------------------------------------------------------------
    #[test]
    fn test_checksum_verified_flag() {
        let mut ctx = DownloadContext::new_default();
        assert!(!ctx.is_checksum_verification_available());
        // By default not verified (but also not available, so "needed" is false)
        assert!(!ctx.is_checksum_verification_needed());

        ctx.set_digest("sha-256".to_string(), "abc".to_string());
        // Available and not verified -> needed (no piece hash type)
        assert!(ctx.is_checksum_verification_needed());

        ctx.set_checksum_verified(true);
        assert!(!ctx.is_checksum_verification_needed());
    }

    // -----------------------------------------------------------------------
    // 19. Signature get/set
    // -----------------------------------------------------------------------
    #[test]
    fn test_signature_get_set() {
        let mut ctx = DownloadContext::new_default();
        assert!(ctx.get_signature().is_none());

        ctx.set_signature(Signature::new(
            "-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----".to_string(),
            "sha-256".to_string(),
        ));

        let sig = ctx.get_signature().unwrap();
        assert_eq!(sig.hash_type, "sha-256");
        assert!(sig.body.contains("BEGIN PGP"));
    }

    // -----------------------------------------------------------------------
    // 20. Timing (resetDownloadStartTime, resetDownloadStopTime, calculateSessionTime)
    // -----------------------------------------------------------------------
    #[test]
    fn test_timing_start_stop_session() {
        let mut ctx = DownloadContext::new_default();

        // Before any timing operations
        assert!(ctx.get_download_stop_time().is_none());
        assert_eq!(ctx.calculate_session_time(), Duration::ZERO);

        // Start
        ctx.reset_download_start_time();
        assert!(ctx.get_net_stat().download_start_time().is_some());

        // Simulate some passage of time
        thread::sleep(Duration::from_millis(50));

        // Stop
        ctx.reset_download_stop_time();
        assert!(ctx.get_download_stop_time().is_some());

        // Session time should be at least 50ms
        let session = ctx.calculate_session_time();
        assert!(session >= Duration::from_millis(50));
    }

    #[test]
    fn test_timing_reset_clears_stop() {
        let mut ctx = DownloadContext::new_default();

        ctx.reset_download_start_time();
        thread::sleep(Duration::from_millis(10));
        ctx.reset_download_stop_time();
        assert!(ctx.get_download_stop_time().is_some());

        // Reset start should clear stop time
        ctx.reset_download_start_time();
        assert!(ctx.get_download_stop_time().is_none());
    }

    // -----------------------------------------------------------------------
    // Attributes
    // -----------------------------------------------------------------------
    #[test]
    fn test_attributes() {
        let mut ctx = DownloadContext::new_default();

        assert!(!ctx.has_attribute(ContextAttributeType::BitTorrent));

        ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(42u64));
        assert!(ctx.has_attribute(ContextAttributeType::BitTorrent));

        let attr = ctx.get_attribute(ContextAttributeType::BitTorrent);
        assert!(attr.is_some());
        let val = attr.unwrap().downcast_ref::<u64>();
        assert!(val.is_some());
        assert_eq!(*val.unwrap(), 42u64);

        assert!(!ctx.has_attribute(ContextAttributeType::Ed2k));
    }

    // -----------------------------------------------------------------------
    // Owner request group ID
    // -----------------------------------------------------------------------
    #[test]
    fn test_owner_request_group_id() {
        let mut ctx = DownloadContext::new_default();
        assert!(ctx.get_owner_request_group_id().is_none());

        ctx.set_owner_request_group_id(42);
        assert_eq!(ctx.get_owner_request_group_id(), Some(42));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------
    #[test]
    #[should_panic(expected = "get_first_file_entry: no file entries")]
    fn test_get_first_file_entry_panics_on_empty() {
        let ctx = DownloadContext::new_default();
        let _ = ctx.get_first_file_entry();
    }

    #[test]
    fn test_num_pieces_with_multiple_files() {
        let mut ctx = DownloadContext::new_default();
        ctx.set_piece_length(1024);
        // Two files: [0, 1000) + [1000, 3000) -> last_offset = 3000
        ctx.set_file_entries(vec![
            make_file_entry("file1.bin", 1000, 0),
            make_file_entry("file2.bin", 2000, 1000),
        ]);
        // (3000 + 1024 - 1) / 1024 = 3
        assert_eq!(ctx.get_num_pieces(), 3);
    }

    #[test]
    fn test_default_trait() {
        let ctx = DownloadContext::default();
        assert_eq!(ctx.get_piece_length(), 0);
        assert!(ctx.knows_total_length());
        assert!(ctx.get_file_entries().is_empty());
    }

    #[test]
    fn test_set_file_entries_replaces() {
        let mut ctx = DownloadContext::new(1024, 4096, "/tmp/old.bin".into());
        assert_eq!(ctx.get_file_entries().len(), 1);

        ctx.set_file_entries(vec![
            make_file_entry("new1.bin", 500, 0),
            make_file_entry("new2.bin", 500, 500),
        ]);
        assert_eq!(ctx.get_file_entries().len(), 2);
        assert_eq!(ctx.get_first_file_entry().path(), "new1.bin");
    }
}
