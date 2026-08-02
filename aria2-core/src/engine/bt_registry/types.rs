//! BtObject and BtObjectBuilder — Data types for the BT registry entries.

use std::fmt;
use std::sync::Arc;

use crate::download::DownloadContext;
use crate::engine::bt_peer_storage::PeerStorage;
use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::bt_tracker_comm::BtAnnounce;
use crate::segment::piece_storage::PieceStorage;

// ===========================================================================
// BtObject
// ===========================================================================

/// Bundles all shared state for a single BitTorrent download.
///
/// Each active BT download gets one `BtObject` entry in the [`BtRegistry`].
/// The object holds shared references (`Arc<>`) for all BT-related components.
///
/// # C++ Equivalence
///
/// | Rust field | C++ field |
/// |---|---|
/// | `download_context` | `shared_ptr<DownloadContext>` |
/// | `piece_storage` | `shared_ptr<PieceStorage>` |
/// | `peer_storage` | `shared_ptr<PeerStorage>` |
/// | `bt_announce` | `shared_ptr<BtAnnounce>` |
/// | `bt_progress_manager` | `shared_ptr<BtProgressInfoFile>` |
///
/// [`BtRegistry`]: super::BtRegistry
pub struct BtObject {
    /// Shared download context (file entries, piece hashes, attributes).
    pub download_context: Option<Arc<DownloadContext>>,

    /// Shared piece storage for this download (trait object for polymorphism).
    /// C++ uses `shared_ptr<PieceStorage>`.
    pub piece_storage: Option<Arc<dyn PieceStorage>>,

    /// Shared peer storage for this download (trait object for polymorphism).
    /// C++ uses `shared_ptr<PeerStorage>`.
    pub peer_storage: Option<Arc<dyn PeerStorage>>,

    /// Shared BT announce handler for this download.
    /// C++ uses `shared_ptr<BtAnnounce>`.
    pub bt_announce: Option<Arc<BtAnnounce>>,

    /// Shared BT progress manager for this download.
    /// Equivalent to C++ `shared_ptr<BtProgressInfoFile>`.
    pub bt_progress_manager: Option<Arc<BtProgressManager>>,
}

impl BtObject {
    /// Create a new `BtObject` with all fields set to `None`.
    pub fn new() -> Self {
        Self {
            download_context: None,
            piece_storage: None,
            peer_storage: None,
            bt_announce: None,
            bt_progress_manager: None,
        }
    }

    /// Return a builder for constructing a `BtObject` with specific fields.
    pub fn builder() -> BtObjectBuilder {
        BtObjectBuilder::default()
    }
}

impl Default for BtObject {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for BtObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BtObject")
            .field(
                "download_context",
                &self.download_context.as_ref().map(|ctx| {
                    format!("<DownloadContext piece_length={}>", ctx.get_piece_length())
                }),
            )
            .field(
                "piece_storage",
                &self.piece_storage.as_ref().map(|_| "<PieceStorage>"),
            )
            .field(
                "peer_storage",
                &self.peer_storage.as_ref().map(|_| "<PeerStorage>"),
            )
            .field(
                "bt_announce",
                &self.bt_announce.as_ref().map(|_| "<BtAnnounce>"),
            )
            .field(
                "bt_progress_manager",
                &self
                    .bt_progress_manager
                    .as_ref()
                    .map(|_| "<BtProgressManager>"),
            )
            .finish()
    }
}

// ===========================================================================
// BtObjectBuilder
// ===========================================================================

/// Builder for constructing a [`BtObject`] with specific fields set.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use aria2_core::engine::bt_registry::BtObject;
/// use aria2_core::download::DownloadContext;
/// use aria2_core::engine::bt_tracker_comm::BtAnnounce;
///
/// let ctx = Arc::new(DownloadContext::new(1024, 4096, "/tmp/file.bin".into()));
/// let bt_announce = Arc::new(BtAnnounce::new(&[], &Some("http://tracker.example/announce".to_string())));
/// let obj = BtObject::builder()
///     .download_context(ctx)
///     .bt_announce(bt_announce)
///     .build();
/// ```
#[derive(Default)]
pub struct BtObjectBuilder {
    download_context: Option<Arc<DownloadContext>>,
    piece_storage: Option<Arc<dyn PieceStorage>>,
    peer_storage: Option<Arc<dyn PeerStorage>>,
    bt_announce: Option<Arc<BtAnnounce>>,
    bt_progress_manager: Option<Arc<BtProgressManager>>,
}

impl BtObjectBuilder {
    /// Set the download context.
    pub fn download_context(mut self, ctx: Arc<DownloadContext>) -> Self {
        self.download_context = Some(ctx);
        self
    }

    /// Set the piece storage.
    pub fn piece_storage(mut self, storage: Arc<dyn PieceStorage>) -> Self {
        self.piece_storage = Some(storage);
        self
    }

    /// Set the peer storage.
    pub fn peer_storage(mut self, storage: Arc<dyn PeerStorage>) -> Self {
        self.peer_storage = Some(storage);
        self
    }

    /// Set the BT announce handler.
    pub fn bt_announce(mut self, announce: Arc<BtAnnounce>) -> Self {
        self.bt_announce = Some(announce);
        self
    }

    /// Set the BT progress manager.
    pub fn bt_progress_manager(mut self, mgr: Arc<BtProgressManager>) -> Self {
        self.bt_progress_manager = Some(mgr);
        self
    }

    /// Build the `BtObject` from the configured fields.
    pub fn build(self) -> BtObject {
        BtObject {
            download_context: self.download_context,
            piece_storage: self.piece_storage,
            peer_storage: self.peer_storage,
            bt_announce: self.bt_announce,
            bt_progress_manager: self.bt_progress_manager,
        }
    }
}
