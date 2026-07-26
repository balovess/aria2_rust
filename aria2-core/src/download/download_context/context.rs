//! DownloadContext — central metadata binding file entries, URIs, and download metadata.

use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::{debug, trace};

use super::net_stat::NetStat;
use super::types::{ContextAttributeType, Signature, TorrentAttribute};
use crate::download::file_entry::FileEntry;

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

    // -- BT info hash (20 bytes, hex-encoded for RPC). Empty for non-BT. --
    info_hash: String,
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
            .field("info_hash", &self.info_hash)
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
            info_hash: String::new(),
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
            info_hash: String::new(),
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
        self.file_entries
            .iter()
            .filter(|fe| fe.is_requested())
            .count()
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
        self.piece_hashes
            .get(index)
            .map(|s| s.as_str())
            .unwrap_or(EMPTY_STRING)
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
    // BT Info Hash
    // -----------------------------------------------------------------------

    /// Return the BT info hash as a hex string. Empty for non-BT downloads.
    /// Mirrors C++ `DownloadContext::getInfoHash()`.
    pub fn info_hash_hex(&self) -> Option<String> {
        if self.info_hash.is_empty() {
            None
        } else {
            Some(self.info_hash.clone())
        }
    }

    /// Set the BT info hash from a hex string.
    pub fn set_info_hash(&mut self, hash: String) {
        self.info_hash = hash;
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
        debug!(
            count = self.file_entries.len(),
            "Runtime resources released"
        );
    }
}

impl Default for DownloadContext {
    fn default() -> Self {
        Self::new_default()
    }
}
