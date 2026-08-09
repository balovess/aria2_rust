// ---------------------------------------------------------------------------
// BtCheckIntegrity — replaces C++ BtCheckIntegrityEntry (implied)
// ---------------------------------------------------------------------------

use std::sync::Arc;

use super::{PieceHashValidator, ValidatorKind};

/// Integrity checking entry for BitTorrent downloads.
///
/// BitTorrent integrity checking differs from stream downloads in two key ways:
///
/// 1. **`on_download_incomplete()`**: BT downloads do NOT proceed to file
///    allocation after integrity check — BT has its own piece management.
/// 2. **`should_report_incomplete_as_error()`**: Returns `false` because
///    incomplete pieces in BT are expected during partial seeding.
///
/// The C++ version does not have an explicit `BtCheckIntegrityEntry` class;
/// BT integrity checking is handled differently in the command pipeline.
/// We provide this struct for a clean separation of concerns.
///
/// # Direct Arc<> References
///
/// Holds `Arc<DownloadContext>` and `Arc<dyn PieceStorage>` directly, matching
/// C++ `shared_ptr<DownloadContext>` and `shared_ptr<PieceStorage>`. This
/// replaces the previous ID-based reference pattern.
pub struct BtCheckIntegrity {
    /// The validator assigned to this entry.
    validator: ValidatorKind,
    /// Shared download context containing piece hashes and metadata.
    /// C++ uses `shared_ptr<DownloadContext>`.
    download_context: Option<Arc<crate::download::DownloadContext>>,
    /// Shared piece storage for data reading and bitfield updates.
    /// C++ uses `shared_ptr<PieceStorage>`.
    piece_storage: Option<Arc<dyn crate::segment::piece_storage::PieceStorage>>,
}

impl std::fmt::Debug for BtCheckIntegrity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtCheckIntegrity")
            .field("validator", &self.validator)
            .field("download_context", &self.download_context.is_some())
            .field("piece_storage", &self.piece_storage.is_some())
            .finish()
    }
}

impl BtCheckIntegrity {
    /// Create a new `BtCheckIntegrity` entry.
    ///
    /// # Arguments
    ///
    /// * `download_context` — Shared DownloadContext containing piece hashes.
    /// * `piece_storage` — Shared PieceStorage for data reading and bitfield updates.
    pub fn new(
        download_context: Arc<crate::download::DownloadContext>,
        piece_storage: Arc<dyn crate::segment::piece_storage::PieceStorage>,
    ) -> Self {
        Self {
            validator: ValidatorKind::None,
            download_context: Some(download_context),
            piece_storage: Some(piece_storage),
        }
    }

    /// Whether the validation is ready to begin.
    ///
    /// For BT downloads, this checks if piece hash verification is available
    /// in the DownloadContext (BT always has piece hashes from the torrent metadata).
    pub fn is_validation_ready(&self) -> bool {
        if let Some(ctx) = &self.download_context {
            !ctx.get_piece_hashes().is_empty()
        } else {
            // BT downloads always have piece hashes from the .torrent metadata.
            true
        }
    }

    /// Initialize the validator for chunk-by-chunk processing.
    ///
    /// Retrieves all metadata (total_pieces, total_length, piece_length) from
    /// the stored `Arc<DownloadContext>`, eliminating the need for external
    /// parameters.
    pub fn init_validator(&mut self) {
        tracing::trace!("BtCheckIntegrity initializing validator");

        if let (Some(ctx), Some(ps)) = (&self.download_context, &self.piece_storage) {
            let total_pieces = ctx.get_piece_hashes().len();
            if total_pieces == 0 {
                tracing::trace!("No piece hashes available — skipping validator creation");
                return;
            }
            let total_length = ctx.get_total_length();
            let piece_length = ctx.get_piece_length() as u64;

            let mut validator = PieceHashValidator::new(
                Arc::clone(ctx),
                Arc::clone(ps),
                total_pieces,
                total_length,
                piece_length,
            );
            validator.init();
            self.validator = ValidatorKind::PieceHash(validator);
        }
    }

    /// Validate a single chunk, delegating to the underlying validator.
    pub fn validate_chunk(&mut self) {
        self.validator.validate_chunk();
    }

    /// Whether the integrity check has completed.
    pub fn is_finished(&self) -> bool {
        self.validator.is_finished()
    }

    /// Total byte length of the data being validated.
    pub fn total_length(&self) -> u64 {
        self.validator.total_length()
    }

    /// Current validated byte length (equivalent to current offset).
    pub fn current_length(&self) -> u64 {
        self.validator.current_offset()
    }

    /// Called when the download finishes successfully after integrity check.
    ///
    /// For BT downloads, this is a no-op. The BT pipeline handles completion
    /// through its own command chain (e.g., seeding mode transition).
    // TODO: Wire up command dispatch when command system is implemented.
    pub fn on_download_finished(&self) {
        tracing::trace!("BtCheckIntegrity::on_download_finished (no-op)");
    }

    /// Called when the download is incomplete after integrity check.
    ///
    /// For BT downloads, this signals that some pieces failed verification.
    /// Unlike stream downloads, BT does NOT proceed to file allocation —
    /// the BT pipeline re-downloads missing pieces through its own mechanism.
    // TODO: Wire up PieceStorage::onDownloadIncomplete() via registry.
    pub fn on_download_incomplete(&self) {
        tracing::trace!("BtCheckIntegrity::on_download_incomplete");
        // C++: ps->onDownloadIncomplete()
        // No file allocation for BT downloads.
    }

    /// Cut trailing garbage data beyond the expected total length.
    // TODO: Wire up DiskAdaptor::cutTrailingGarbage() via registry.
    pub fn cut_trailing_garbage(&self) {
        tracing::trace!("BtCheckIntegrity::cut_trailing_garbage");
    }

    /// Whether incomplete validation should be reported as an error.
    ///
    /// Returns `false` for BT downloads — incomplete pieces are expected
    /// during partial seeding and the BT pipeline handles re-downloading.
    pub fn should_report_incomplete_as_error(&self) -> bool {
        false
    }

    /// Return a reference to the underlying validator.
    pub fn validator(&self) -> &ValidatorKind {
        &self.validator
    }

    /// Return a mutable reference to the underlying validator.
    pub fn validator_mut(&mut self) -> &mut ValidatorKind {
        &mut self.validator
    }
}
