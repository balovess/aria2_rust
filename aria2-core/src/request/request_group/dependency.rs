//! Download dependency resolution.
//!
//! Mirrors C++ `Dependency` class hierarchy. A `Dependency` represents a
//! condition that must be satisfied before a `RequestGroup` can be promoted
//! from reserved to active. The most common dependency is "wait for another
//! download to finish" (e.g. Metalink → torrent download chains).

use std::sync::Arc;

#[cfg(feature = "bittorrent")]
use super::DownloadStatus;
use super::GroupId;
#[cfg(feature = "bittorrent")]
use super::{MetadataInfo, RequestGroup};
#[cfg(feature = "bittorrent")]
use crate::engine::bt_download_command::build_download_context_from_meta;
#[cfg(feature = "bittorrent")]
use crate::util::rwlock_ext::RwLockRecover;
#[cfg(feature = "bittorrent")]
use std::path::PathBuf;
#[cfg(feature = "bittorrent")]
use std::sync::RwLock;

/// A dependency that must be resolved before a download can start.
///
/// Mirrors the C++ `Dependency` base class and its `virtual bool resolve()`
/// method. In Rust, a trait object allows each dependency type to define
/// its own resolution logic.
///
/// The trait also exposes `Any` support for downcasting in the engine loop,
/// such as finding `CompletionDependency` instances when their prerequisite
/// group completes.
pub trait Dependency: Send + Sync + std::fmt::Debug + std::any::Any {
    /// Check whether this dependency has been resolved.
    ///
    /// Returns `true` if the dependency is satisfied and the download
    /// can proceed, `false` if it must remain in the reserved queue.
    fn resolve(&self) -> bool;

    /// Human-readable description of this dependency for logging.
    fn description(&self) -> String;

    /// Support for downcasting. Required for the engine loop to find
    /// specific dependency types (e.g. `CompletionDependency`) in the
    /// reserved queue.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Dependency on another download completing.
///
/// The dependent download waits in the reserved queue until the
/// prerequisite download finishes. This is used by:
/// - Metalink: parent Metalink download → child torrent downloads
/// - Torrent → magnet: magnet link download triggers torrent download
///
/// Mirrors C++ `DownloadResultDependency` and `GIDDependency`.
#[derive(Debug)]
pub struct CompletionDependency {
    /// GID of the prerequisite download.
    pub depends_on_gid: GroupId,
    /// Shared flag that gets set when the prerequisite completes.
    /// Allows lock-free resolution checking from the promotion path.
    completed: Arc<std::sync::atomic::AtomicBool>,
}

impl CompletionDependency {
    /// Create a new completion dependency on the given GID.
    ///
    /// The `completed` flag starts as `false` and should be set to `true`
    /// by the engine loop when the prerequisite group is demoted to stopped.
    pub fn new(depends_on_gid: GroupId) -> Self {
        Self {
            depends_on_gid,
            completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Get a shared reference to the completion flag.
    ///
    /// The engine loop uses this to mark the dependency as resolved
    /// when the prerequisite download finishes.
    pub fn completed_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.completed)
    }

    /// Manually mark this dependency as resolved (for testing).
    pub fn mark_resolved(&self) {
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// A completion dependency that installs a parsed torrent context into the
/// payload group before allowing promotion.
#[cfg(feature = "bittorrent")]
pub struct BtDependency {
    depends_on_gid: GroupId,
    completed: Arc<std::sync::atomic::AtomicBool>,
    payload: Arc<RwLock<RequestGroup>>,
    torrent_data: Arc<RwLock<Option<Vec<u8>>>>,
    metadata_path: Option<PathBuf>,
    output_path: PathBuf,
    metadata_info: MetadataInfo,
    /// Direct HTTP/FTP mirrors to use when the torrent metaurl fails.
    fallback_uris: Vec<String>,
}

#[cfg(feature = "bittorrent")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtDependencyResolution {
    /// Metadata was parsed and injected, or direct mirrors were selected.
    Resolved,
    /// The prerequisite is not terminal yet.
    Waiting,
    /// No usable fallback exists for the payload.
    Failed(String),
}

#[cfg(feature = "bittorrent")]
impl std::fmt::Debug for BtDependency {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtDependency")
            .field("depends_on_gid", &self.depends_on_gid)
            .field("payload_gid", &self.payload.recover().gid())
            .field("metadata_path", &self.metadata_path)
            .field("output_path", &self.output_path)
            .field("metadata_info", &self.metadata_info)
            .field("fallback_uris", &self.fallback_uris)
            .finish()
    }
}

#[cfg(feature = "bittorrent")]
impl BtDependency {
    /// Return the prerequisite metadata task GID.
    pub fn depends_on_gid(&self) -> GroupId {
        self.depends_on_gid
    }

    /// Create a dependency for a payload group and its downloaded torrent data.
    pub fn new(
        depends_on_gid: GroupId,
        payload: Arc<RwLock<RequestGroup>>,
        torrent_data: Vec<u8>,
        output_path: PathBuf,
        metadata_info: MetadataInfo,
    ) -> Self {
        Self::from_source(
            depends_on_gid,
            payload,
            Some(torrent_data),
            None,
            output_path,
            metadata_info,
            Vec::new(),
        )
    }

    /// Create a dependency whose metadata will be read after the prerequisite
    /// task writes its torrent file.
    pub fn new_file(
        depends_on_gid: GroupId,
        payload: Arc<RwLock<RequestGroup>>,
        metadata_path: PathBuf,
        output_path: PathBuf,
        metadata_info: MetadataInfo,
    ) -> Self {
        Self::from_source(
            depends_on_gid,
            payload,
            None,
            Some(metadata_path),
            output_path,
            metadata_info,
            Vec::new(),
        )
    }

    /// Create a file-backed dependency with direct mirrors available as a
    /// fallback when the torrent metadata cannot be downloaded or parsed.
    pub fn new_file_with_fallback(
        depends_on_gid: GroupId,
        payload: Arc<RwLock<RequestGroup>>,
        metadata_path: PathBuf,
        output_path: PathBuf,
        metadata_info: MetadataInfo,
        fallback_uris: Vec<String>,
    ) -> Self {
        Self::from_source(
            depends_on_gid,
            payload,
            None,
            Some(metadata_path),
            output_path,
            metadata_info,
            fallback_uris,
        )
    }

    fn from_source(
        depends_on_gid: GroupId,
        payload: Arc<RwLock<RequestGroup>>,
        torrent_data: Option<Vec<u8>>,
        metadata_path: Option<PathBuf>,
        output_path: PathBuf,
        metadata_info: MetadataInfo,
        fallback_uris: Vec<String>,
    ) -> Self {
        Self {
            depends_on_gid,
            completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            payload,
            torrent_data: Arc::new(RwLock::new(torrent_data)),
            metadata_path,
            output_path,
            metadata_info,
            fallback_uris,
        }
    }

    /// Configure the path written by the prerequisite metadata task.
    pub fn set_metadata_path(&mut self, path: PathBuf) {
        self.metadata_path = Some(path);
    }

    /// Return the configured metadata file path.
    pub fn metadata_path(&self) -> Option<&std::path::Path> {
        self.metadata_path.as_deref()
    }

    /// Resolve the dependency after its prerequisite reaches a terminal
    /// state. A failed metadata path is recoverable only when direct mirrors
    /// were retained by the Metalink graph.
    pub fn resolve_after_prerequisite(
        &self,
        prerequisite_status: &DownloadStatus,
    ) -> BtDependencyResolution {
        if self.resolve() {
            return BtDependencyResolution::Resolved;
        }

        if matches!(prerequisite_status, DownloadStatus::Complete) {
            if let Some(metadata_path) = self.metadata_path()
                && let Err(error) = self.resolve_metadata_file(metadata_path)
            {
                return self.fallback_or_fail(error);
            }
            return if self.resolve() {
                BtDependencyResolution::Resolved
            } else {
                self.fallback_or_fail("torrent metadata source is unavailable".to_string())
            };
        }

        self.fallback_or_fail(format!(
            "metadata prerequisite ended in {}",
            prerequisite_status
        ))
    }

    fn fallback_or_fail(&self, reason: String) -> BtDependencyResolution {
        if !self.fallback_uris.is_empty() {
            self.payload
                .recover_mut()
                .replace_uris(self.fallback_uris.clone());
            self.completed
                .store(true, std::sync::atomic::Ordering::Release);
            tracing::warn!(
                payload_gid = self.payload.recover().gid().value(),
                error = %reason,
                fallback_count = self.fallback_uris.len(),
                "Torrent metadata failed; falling back to direct Metalink mirrors"
            );
            BtDependencyResolution::Resolved
        } else {
            BtDependencyResolution::Failed(reason)
        }
    }

    /// Resolve downloaded metadata from the file written by the prerequisite group.
    pub fn resolve_metadata_file(&self, path: &std::path::Path) -> Result<(), String> {
        if self.resolve() {
            return Ok(());
        }
        let data = std::fs::read(path).map_err(|error| {
            format!(
                "Failed to read torrent metadata '{}': {error}",
                path.display()
            )
        })?;
        self.resolve_metadata_bytes(data)
    }

    /// Mark metadata complete and resolve it immediately when the torrent is valid.
    pub fn mark_metadata_complete(&self) -> Result<(), String> {
        if self.resolve() {
            return Ok(());
        }

        let data = self
            .torrent_data
            .recover()
            .as_ref()
            .cloned()
            .ok_or_else(|| "Torrent metadata source is file-backed".to_string())?;
        self.resolve_metadata_bytes(data)
    }

    fn resolve_metadata_bytes(&self, data: Vec<u8>) -> Result<(), String> {
        // Validate the metadata before consuming the source so callers can
        // retry after a parse failure.
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&data)
            .map_err(|error| format!("Torrent metadata parse failed: {error}"))?;
        if self.torrent_data.recover().is_some() {
            self.torrent_data
                .recover_mut()
                .take()
                .ok_or_else(|| "Torrent metadata has already been consumed".to_string())?;
        }
        let ctx = build_download_context_from_meta(
            &meta,
            self.output_path.to_string_lossy().into_owned(),
        )
        .map_err(|error| error.to_string())?;
        let payload = self.payload.recover();
        payload.set_bt_metadata(
            meta.num_pieces() as u32,
            meta.info.piece_length,
            meta.info_hash.as_hex(),
        );
        payload.set_metadata_info(self.metadata_info.clone());
        payload.set_download_context(Arc::new(ctx));
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

#[cfg(feature = "bittorrent")]
impl Dependency for BtDependency {
    fn resolve(&self) -> bool {
        self.completed.load(std::sync::atomic::Ordering::Acquire)
    }

    fn description(&self) -> String {
        format!(
            "Waiting for torrent metadata download #{}",
            self.depends_on_gid.to_hex_string()
        )
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Dependency for CompletionDependency {
    fn resolve(&self) -> bool {
        self.completed.load(std::sync::atomic::Ordering::Acquire)
    }

    fn description(&self) -> String {
        format!(
            "Waiting for download #{} to complete",
            self.depends_on_gid.to_hex_string()
        )
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A dependency that is always resolved (no-op).
///
/// Used as a default when a group has no dependencies.
#[derive(Debug)]
pub struct NoDependency;

impl Dependency for NoDependency {
    fn resolve(&self) -> bool {
        true
    }

    fn description(&self) -> String {
        "No dependency".to_string()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "bittorrent")]
    use crate::request::request_group::{DownloadOptions, MetadataInfo, RequestGroup};
    #[cfg(feature = "bittorrent")]
    use crate::util::rwlock_ext::RwLockRecover;
    #[cfg(feature = "bittorrent")]
    use std::path::PathBuf;
    #[cfg(feature = "bittorrent")]
    use std::sync::{Arc, RwLock};

    #[cfg(feature = "bittorrent")]
    fn minimal_torrent() -> Vec<u8> {
        let mut data = b"d8:announce28:http://tracker.test/announce4:infod6:lengthi0e4:name4:test12:piece lengthi16384e6:pieces20:".to_vec();
        data.extend_from_slice(&[
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
        ]);
        data.extend_from_slice(b"ee");
        data
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bt_dependency_reads_file_backed_metadata() {
        let path = std::env::temp_dir().join(format!(
            "aria2-rust-bt-dependency-{}.torrent",
            std::process::id()
        ));
        std::fs::write(&path, minimal_torrent()).expect("write torrent metadata");
        let payload = Arc::new(RwLock::new(RequestGroup::new(
            GroupId::new(3),
            vec!["bt://payload".to_string()],
            DownloadOptions::default(),
        )));
        let dependency = BtDependency::new_file(
            GroupId::new(1),
            Arc::clone(&payload),
            path.clone(),
            PathBuf::from("payload.bin"),
            MetadataInfo::new(GroupId::new(1), "https://example.test/payload.torrent"),
        );

        dependency
            .resolve_metadata_file(&path)
            .expect("file-backed metadata should resolve");
        assert!(dependency.resolve());
        assert!(payload.recover().get_download_context().is_some());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bt_dependency_injects_context_before_resolution() {
        let payload = Arc::new(RwLock::new(RequestGroup::new(
            GroupId::new(2),
            vec!["bt://payload".to_string()],
            DownloadOptions::default(),
        )));
        let dependency = BtDependency::new(
            GroupId::new(1),
            Arc::clone(&payload),
            minimal_torrent(),
            PathBuf::from("payload.bin"),
            MetadataInfo::new(GroupId::new(1), "https://example.test/payload.torrent"),
        );

        assert!(!dependency.resolve());
        dependency
            .mark_metadata_complete()
            .expect("valid torrent metadata should resolve");
        assert!(dependency.resolve());
        let group = payload.recover();
        assert_eq!(group.get_bt_piece_length(), 16_384);
        assert!(group.get_download_context().is_some());
        assert_eq!(
            group.metadata_info().expect("metadata provenance").gid(),
            Some(GroupId::new(1))
        );
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bt_dependency_falls_back_after_failed_prerequisite() {
        let payload = Arc::new(RwLock::new(RequestGroup::new(
            GroupId::new(12),
            vec!["bt://payload".to_string()],
            DownloadOptions::default(),
        )));
        let dependency = BtDependency::new_file_with_fallback(
            GroupId::new(11),
            Arc::clone(&payload),
            PathBuf::from("missing.torrent"),
            PathBuf::from("payload.bin"),
            MetadataInfo::new(GroupId::new(11), "https://example.test/payload.torrent"),
            vec!["https://mirror.test/payload.bin".to_string()],
        );

        assert_eq!(
            dependency.resolve_after_prerequisite(&DownloadStatus::Error(
                "metadata request failed".to_string()
            )),
            BtDependencyResolution::Resolved
        );
        assert!(dependency.resolve());
        assert_eq!(
            payload.recover().uris(),
            &["https://mirror.test/payload.bin".to_string()]
        );
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bt_dependency_without_fallback_reports_terminal_failure() {
        let payload = Arc::new(RwLock::new(RequestGroup::new(
            GroupId::new(13),
            vec!["bt://payload".to_string()],
            DownloadOptions::default(),
        )));
        let dependency = BtDependency::new_file(
            GroupId::new(11),
            Arc::clone(&payload),
            PathBuf::from("missing.torrent"),
            PathBuf::from("payload.bin"),
            MetadataInfo::new(GroupId::new(11), "https://example.test/payload.torrent"),
        );

        assert!(matches!(
            dependency.resolve_after_prerequisite(&DownloadStatus::Error(
                "metadata request failed".to_string()
            )),
            BtDependencyResolution::Failed(_)
        ));
        assert!(!dependency.resolve());
    }

    #[test]
    fn test_no_dependency_always_resolved() {
        let dep = NoDependency;
        assert!(dep.resolve());
        assert_eq!(dep.description(), "No dependency");
    }

    #[test]
    fn test_completion_dependency_initially_unresolved() {
        let dep = CompletionDependency::new(GroupId::new(1));
        assert!(!dep.resolve());
        assert!(dep.description().contains("#0000000000000001"));
    }

    #[test]
    fn test_completion_dependency_resolved_after_mark() {
        let dep = CompletionDependency::new(GroupId::new(42));
        assert!(!dep.resolve());

        dep.mark_resolved();
        assert!(dep.resolve());
    }

    #[test]
    fn test_completion_dependency_shared_flag() {
        let dep = CompletionDependency::new(GroupId::new(1));
        let flag = dep.completed_flag();

        assert!(!dep.resolve());

        // Setting the flag from a different reference resolves the dependency.
        flag.store(true, std::sync::atomic::Ordering::Release);
        assert!(dep.resolve());
    }
}
