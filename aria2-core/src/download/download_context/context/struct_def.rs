//! DownloadContext struct definition, Debug impl, and constructors.

use std::any::Any;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use super::super::net_stat::NetStat;
use super::super::types::{ContextAttributeType, Signature};
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
    pub(super) signature: Option<Signature>,

    // -- Back-pointer to owning RequestGroup (ID-based, not a raw pointer) --
    pub(super) owner_request_group_id: Option<u64>,

    // -- Typed attribute extension map --
    pub(super) attrs: HashMap<ContextAttributeType, Box<dyn Any + Send + Sync>>,

    // -- Ordered list of files in this download --
    pub(super) file_entries: Vec<FileEntry>,

    // -- Per-piece hash values for verification --
    pub(super) piece_hashes: Vec<String>,

    // -- Per-download network statistics --
    pub(super) net_stat: NetStat,

    // -- Optional manager-owned aggregate network statistics --
    pub(super) global_net_stat:
        OnceLock<std::sync::Arc<crate::request::global_net_stat::GlobalNetStat>>,

    // -- Timestamp when download stopped --
    pub(super) download_stop_time: Option<std::time::Instant>,

    // -- Hash algorithm name for piece hashes --
    pub(super) piece_hash_type: String,

    // -- Whole-file hash digest value --
    pub(super) digest: String,

    // -- Whole-file hash algorithm name --
    pub(super) hash_type: String,

    // -- Override path for .aria2 control file naming --
    pub(super) base_path: String,

    // -- Piece length in bytes (0 = unknown) --
    pub(super) piece_length: u32,

    // -- Whether the whole-file checksum has already been verified --
    pub(super) checksum_verified: AtomicBool,

    // -- Whether total length is known --
    pub(super) knows_total_length: bool,

    // -- Whether to parse Metalink info from response headers --
    pub(super) accept_metalink: bool,

    // -- BT info hash (20 bytes, hex-encoded for RPC). Empty for non-BT. --
    pub(super) info_hash: String,
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
            .field(
                "checksum_verified",
                &self
                    .checksum_verified
                    .load(std::sync::atomic::Ordering::Acquire),
            )
            .field("knows_total_length", &self.knows_total_length)
            .field("accept_metalink", &self.accept_metalink)
            .field("info_hash", &self.info_hash)
            .finish()
    }
}

// Static empty string for returning references to "no hash" without allocation.
pub(super) static EMPTY_STRING: &str = "";

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
            global_net_stat: OnceLock::new(),
            download_stop_time: None,
            piece_hash_type: String::new(),
            digest: String::new(),
            hash_type: String::new(),
            base_path: String::new(),
            piece_length: 0,
            checksum_verified: AtomicBool::new(false),
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
            global_net_stat: OnceLock::new(),
            download_stop_time: None,
            piece_hash_type: String::new(),
            digest: String::new(),
            hash_type: String::new(),
            base_path: String::new(),
            checksum_verified: AtomicBool::new(false),
            knows_total_length: true,
            accept_metalink: true,
            info_hash: String::new(),
        }
    }
}

impl Default for DownloadContext {
    fn default() -> Self {
        Self::new_default()
    }
}
