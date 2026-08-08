//! RequestGroup struct definition and constructor.
//!
//! Each `RequestGroup` represents one download task, tracking its URIs,
//! status, progress, segments, and lifecycle control flags. Methods are
//! split across sub-modules for cohesion and to respect the 600-line
//! file size limit.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};

use tracing::info;

use crate::download::DownloadContext;
use crate::rate_limiter::RateLimiter;
use crate::segment::Segment;

use super::bt_peer_snapshot::BtPeerSnapshot;
use super::group_id::GroupId;
use super::halt_reason::{DownloadControlFlags, HaltReason};
use super::options::DownloadOptions;
use super::progress::AtomicProgress;
use super::result_code::DownloadResultCode;

/// A single download task managed by the engine.
///
/// Each `RequestGroup` owns:
/// - The initial URI list (transferred to `FileEntry` when `DownloadContext` is set)
/// - The current download status and control flags
/// - Lock-free progress counters for hot-path updates
/// - An optional `DownloadContext` with `FileEntry` objects that manage
///   the URI lifecycle (remaining → spent → results)
///
/// # Thread Safety
///
/// `RequestGroup` is `Send + Sync` — it uses `RwLock` and atomics for
/// interior mutability. It is typically shared via `Arc<RwLock<RequestGroup>>`
/// between the engine loop and RPC handlers.
///
/// # URI Lifecycle
///
/// URIs follow a 3-tier state machine (matching C++ `FileEntry`):
///
/// ```text
/// RequestGroup.uris (initial) → FileEntry.remaining_uris → FileEntry.spent_uris → FileEntry.uri_results
/// ```
///
/// When `set_download_context()` is called, the initial URIs are transferred
/// to the first `FileEntry`'s `remaining_uris`. After that, all URI lifecycle
/// operations delegate to `FileEntry` via `DownloadContext`.
pub struct RequestGroup {
    /// Group identifier — unique across the engine session.
    pub(super) gid: GroupId,
    /// Initial URI list provided at construction time.
    ///
    /// These URIs are transferred to the first `FileEntry`'s `remaining_uris`
    /// when `set_download_context()` is called. After transfer, this field
    /// is still available for RPC queries that need the original URI list,
    /// but URI lifecycle operations should go through `DownloadContext`.
    pub(super) uris: Vec<String>,
    /// Metalink file name override, independent of the global `out` option.
    pub(super) output_name: std::sync::RwLock<Option<String>>,
    /// Download options — shared via `Arc` for cheap cloning.
    pub(super) options: Arc<DownloadOptions>,
    /// Successfully applied task-level runtime overrides.
    ///
    /// The typed fields in [`DownloadOptions`] remain the execution source for
    /// implemented options. This map preserves canonical options that are
    /// validated at the runtime seam but do not yet have a dedicated execution
    /// field, so adapters never report success and silently lose the value.
    pub(super) runtime_options: std::sync::RwLock<HashMap<String, serde_json::Value>>,
    /// Options deferred until the current command generation is restarted.
    pub pending_options: std::sync::RwLock<HashMap<String, serde_json::Value>>,
    /// Current download status.
    pub(super) status: std::sync::RwLock<super::status::DownloadStatus>,
    /// Allocated download segments.
    pub(super) segments: std::sync::RwLock<Vec<Segment>>,
    /// Timestamp when the download started.
    pub(super) start_time: std::sync::RwLock<Option<std::time::Instant>>,
    /// Timestamp when the download ended (completed, errored, or removed).
    pub(super) end_time: std::sync::RwLock<Option<std::time::Instant>>,

    /// Lock-free progress counters shared via `Arc` so the hot-path download
    /// code can update progress without acquiring the outer `RwLock`.
    pub progress: Arc<AtomicProgress>,
    /// BT piece bitfield.
    pub bt_bitfield: std::sync::RwLock<Option<Vec<u8>>>,
    /// Current active BT peer snapshots for read-only consumers.
    pub bt_peer_snapshots: std::sync::RwLock<Vec<BtPeerSnapshot>>,

    /// Download context — central metadata (file entries, piece hashes, attributes).
    /// In C++ aria2, `RequestGroup` owns `shared_ptr<DownloadContext> dctx_`.
    /// `None` until the download engine populates it.
    pub download_context: std::sync::RwLock<Option<Arc<DownloadContext>>>,

    // BT metadata fields (for session persistence enhancement)
    /// Number of pieces in the torrent (0 for non-BT downloads).
    pub bt_num_pieces: AtomicU32,
    /// Size of each piece in bytes (0 for non-BT downloads).
    pub bt_piece_length: AtomicU32,
    /// Info hash hex string for torrent identification (None for non-BT).
    pub bt_info_hash_hex: std::sync::RwLock<Option<String>>,

    /// Handle to the download's `RateLimiter` for dynamic rate adjustment.
    /// `None` until the download engine wires up a `ThrottledWriter`.
    pub rate_limiter: std::sync::RwLock<Option<RateLimiter>>,

    // ── Lifecycle control flags (C++ haltRequested_/forceHaltRequested_/pauseRequested_) ──
    /// Number of in-flight command tasks for this group.
    /// When this reaches 0, the group can be demoted from active to stopped.
    pub num_commands: AtomicU32,

    /// Atomic control flags for halt/pause/force-halt/restart signals.
    /// Checked by hot download loops without acquiring the `RwLock` on status.
    pub control_flags: DownloadControlFlags,

    /// Reason why the download was halted.
    pub halt_reason: std::sync::RwLock<HaltReason>,

    /// Last error code recorded for this download.
    pub last_error_code: std::sync::RwLock<DownloadResultCode>,

    /// Last error message recorded for this download.
    pub last_error_message: std::sync::RwLock<String>,

    /// Last real peer connection observed by an HTTP/FTP command.
    /// This is a snapshot only; protocol tasks remain the owners of sockets.
    pub connection_contexts: std::sync::RwLock<Vec<crate::network::ConnectionContext>>,

    /// Whether any command in the current command generation failed.
    pub command_failure: AtomicBool,

    /// Whether saving the .aria2 control file is currently enabled.
    /// Disabled during hash checking to prevent corrupt state.
    pub save_control_file_enabled: std::sync::RwLock<std::sync::atomic::AtomicBool>,

    /// Whether the BitTorrent payload has completed and this group is now
    /// seed-only. Seed-only groups remain available for seeding but do not
    /// consume the normal active-download concurrency budget.
    pub seed_only: AtomicBool,

    /// Output path whose sidecar `.aria2` file belongs to this group.
    pub control_file_path: std::sync::RwLock<Option<std::path::PathBuf>>,

    /// Optional dependency that must be resolved before this group
    /// can be promoted from reserved to active.
    pub dependency: std::sync::RwLock<Option<Arc<dyn super::dependency::Dependency>>>,

    /// GID of the parent download that spawned this one.
    ///
    /// Mirrors C++ `RequestGroup::following_`. Set when a post-download
    /// handler creates child groups (e.g. Metalink → child downloads,
    /// torrent → magnet). Used by RPC `following` field and for
    /// parent-child relationship tracking.
    pub following_gid: std::sync::RwLock<Option<GroupId>>,

    /// GIDs of downloads spawned by this one.
    ///
    /// Mirrors C++ `RequestGroup::followedBy_`. Populated when a
    /// post-download handler creates child groups from this download.
    /// Used by RPC `followedBy` field.
    pub followed_by_gids: std::sync::RwLock<Vec<GroupId>>,

    /// GID of the download this group belongs to.
    ///
    /// Mirrors C++ `RequestGroup::belongsToGID_`, used for child downloads
    /// such as Metalink/torrent follow-up groups.
    pub belongs_to_gid: std::sync::RwLock<Option<GroupId>>,

    /// Provenance of the metadata that created this group.
    ///
    /// Mirrors C++ `RequestGroup::metadataInfo_`. It is intentionally kept
    /// separate from parent/child GIDs because metadata provenance can be
    /// data-only and does not itself imply a generated child.
    pub metadata_info: std::sync::RwLock<Option<super::metadata_info::MetadataInfo>>,

    /// In-memory torrent metadata supplied by a CLI/RPC caller.
    #[cfg(feature = "bittorrent")]
    pub bt_metadata_data: std::sync::RwLock<Option<Vec<u8>>>,

    /// Whether this group uses the C++ `MemoryPreDownloadHandler` semantics.
    ///
    /// This is separate from protocol-specific metadata slots because the
    /// flag describes the source download lifecycle, not the type of bytes
    /// eventually parsed by a post-download handler.
    pub in_memory_download: AtomicBool,
    /// Bytes collected by an in-memory source download.
    pub in_memory_data: std::sync::RwLock<Option<Vec<u8>>>,
    /// Content-Type observed for the source response.
    pub content_type: std::sync::RwLock<Option<String>>,

    /// Raw Metalink document and selected file index for manager-owned fallback execution.
    #[cfg(feature = "metalink")]
    pub metalink_data: std::sync::RwLock<Option<Vec<u8>>>,
    #[cfg(feature = "metalink")]
    pub metalink_file_index: std::sync::RwLock<Option<usize>>,
    /// Base URI used when the Metalink source was parsed.
    #[cfg(feature = "metalink")]
    pub metalink_base_uri: std::sync::RwLock<Option<String>>,
}

impl RequestGroup {
    /// Create a new `RequestGroup` with the given GID, URIs, and options.
    ///
    /// The group starts in `Waiting` status with no in-flight commands.
    /// URIs are stored in the `uris` field and will be transferred to the
    /// first `FileEntry` when `set_download_context()` is called.
    pub fn new(gid: GroupId, uris: Vec<String>, options: DownloadOptions) -> Self {
        info!("Creating request group #{}", gid.value());

        RequestGroup {
            gid,
            uris,
            output_name: std::sync::RwLock::new(None),
            options: Arc::new(options),
            runtime_options: std::sync::RwLock::new(HashMap::new()),
            pending_options: std::sync::RwLock::new(HashMap::new()),
            status: std::sync::RwLock::new(super::status::DownloadStatus::Waiting),
            segments: std::sync::RwLock::new(Vec::new()),
            start_time: std::sync::RwLock::new(None),
            end_time: std::sync::RwLock::new(None),
            progress: Arc::new(AtomicProgress::new()),
            bt_bitfield: std::sync::RwLock::new(None),
            bt_peer_snapshots: std::sync::RwLock::new(Vec::new()),
            download_context: std::sync::RwLock::new(None),
            bt_num_pieces: AtomicU32::new(0),
            bt_piece_length: AtomicU32::new(0),
            bt_info_hash_hex: std::sync::RwLock::new(None),
            rate_limiter: std::sync::RwLock::new(None),
            num_commands: AtomicU32::new(0),
            control_flags: DownloadControlFlags::new(),
            halt_reason: std::sync::RwLock::new(HaltReason::None),
            last_error_code: std::sync::RwLock::new(DownloadResultCode::UnknownError),
            last_error_message: std::sync::RwLock::new(String::new()),
            connection_contexts: std::sync::RwLock::new(Vec::new()),
            command_failure: AtomicBool::new(false),
            save_control_file_enabled: std::sync::RwLock::new(std::sync::atomic::AtomicBool::new(
                true,
            )),
            seed_only: AtomicBool::new(false),
            control_file_path: std::sync::RwLock::new(None),
            dependency: std::sync::RwLock::new(None),
            following_gid: std::sync::RwLock::new(None),
            followed_by_gids: std::sync::RwLock::new(Vec::new()),
            belongs_to_gid: std::sync::RwLock::new(None),
            metadata_info: std::sync::RwLock::new(None),
            #[cfg(feature = "bittorrent")]
            bt_metadata_data: std::sync::RwLock::new(None),
            in_memory_download: AtomicBool::new(false),
            in_memory_data: std::sync::RwLock::new(None),
            content_type: std::sync::RwLock::new(None),
            #[cfg(feature = "metalink")]
            metalink_data: std::sync::RwLock::new(None),
            #[cfg(feature = "metalink")]
            metalink_file_index: std::sync::RwLock::new(None),
            #[cfg(feature = "metalink")]
            metalink_base_uri: std::sync::RwLock::new(None),
        }
    }
}
