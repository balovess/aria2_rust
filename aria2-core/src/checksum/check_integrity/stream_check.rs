// ---------------------------------------------------------------------------
// StreamCheckIntegrity — replaces C++ StreamCheckIntegrityEntry
// ---------------------------------------------------------------------------

use std::sync::Arc;

use super::{PieceHashValidator, ValidatorKind};

/// Integrity checking entry for stream (HTTP/FTP) downloads.
///
/// This is the Rust equivalent of the C++ `StreamCheckIntegrityEntry`, which
/// inherits from `PieceHashCheckIntegrityEntry` which inherits from
/// `CheckIntegrityEntry`. In Rust, we flatten this into a single struct with
/// all necessary fields and methods.
///
/// # C++ Mapping
///
/// | C++ Method                     | Rust Method                  |
/// |--------------------------------|-------------------------------|
/// | `isValidationReady()`          | `is_validation_ready()`       |
/// | `initValidator()`              | `init_validator()`            |
/// | `validateChunk()`              | `validate_chunk()`            |
/// | `finished()`                   | `is_finished()`              |
/// | `getTotalLength()`             | `total_length()`              |
/// | `getCurrentLength()`           | `current_length()`            |
/// | `onDownloadFinished()`         | `on_download_finished()`      |
/// | `onDownloadIncomplete()`       | `on_download_incomplete()`    |
/// | `cutTrailingGarbage()`         | `cut_trailing_garbage()`      |
/// | `shouldReportIncompleteAsError()` | `should_report_incomplete_as_error()` |
pub struct StreamCheckIntegrity {
    /// The validator assigned to this entry.
    /// Equivalent to C++ `CheckIntegrityEntry::validator_`.
    validator: ValidatorKind,
    /// Shared download context containing piece hashes.
    download_context: Option<Arc<crate::download::DownloadContext>>,
    /// Shared piece storage for data reading and bitfield updates.
    piece_storage: Option<Arc<dyn crate::segment::piece_storage::PieceStorage>>,
    /// If true, only perform hash checking and do NOT proceed to file allocation
    /// after the check completes. Matches C++ `PREF_HASH_CHECK_ONLY` option.
    hash_check_only: bool,
}

impl std::fmt::Debug for StreamCheckIntegrity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamCheckIntegrity")
            .field("validator", &self.validator)
            .field("download_context", &self.download_context.is_some())
            .field("piece_storage", &self.piece_storage.is_some())
            .field("hash_check_only", &self.hash_check_only)
            .finish()
    }
}

impl StreamCheckIntegrity {
    /// Create a new `StreamCheckIntegrity` entry.
    ///
    /// # Arguments
    ///
    /// * `download_context` — Shared DownloadContext containing piece hashes.
    /// * `piece_storage` — Shared PieceStorage for data reading and bitfield updates.
    /// * `hash_check_only` — If true, skip file allocation after integrity check.
    pub fn new(
        download_context: Arc<crate::download::DownloadContext>,
        piece_storage: Arc<dyn crate::segment::piece_storage::PieceStorage>,
        hash_check_only: bool,
    ) -> Self {
        Self {
            validator: ValidatorKind::None,
            download_context: Some(download_context),
            piece_storage: Some(piece_storage),
            hash_check_only,
        }
    }

    /// Whether the validation is ready to begin.
    ///
    /// In C++ `PieceHashCheckIntegrityEntry::isValidationReady()`, this checks
    /// `dctx->isPieceHashVerificationAvailable()`. Here we check if the
    /// DownloadContext has piece hashes set.
    pub fn is_validation_ready(&self) -> bool {
        // C++: dctx->isPieceHashVerificationAvailable()
        if let Some(ctx) = &self.download_context {
            !ctx.get_piece_hashes().is_empty()
        } else {
            false
        }
    }

    /// Initialize the validator for chunk-by-chunk processing.
    ///
    /// In C++ `PieceHashCheckIntegrityEntry::initValidator()`, this creates an
    /// `IteratableChunkChecksumValidator` with the DownloadContext and PieceStorage,
    /// calls `init()` on it, and stores it as the validator.
    pub fn init_validator(&mut self) {
        tracing::trace!("StreamCheckIntegrity initializing validator");

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
    ///
    /// Matches C++ `CheckIntegrityEntry::getTotalLength()`.
    pub fn total_length(&self) -> u64 {
        self.validator.total_length()
    }

    /// Current validated byte length (equivalent to current offset).
    ///
    /// Matches C++ `CheckIntegrityEntry::getCurrentLength()`, which delegates
    /// to `validator_->getCurrentOffset()`.
    pub fn current_length(&self) -> u64 {
        self.validator.current_offset()
    }

    /// Called when the download finishes successfully after integrity check.
    ///
    /// In C++ `StreamCheckIntegrityEntry::onDownloadFinished()`, this is a no-op.
    /// The stream download path does not need special handling on success.
    // TODO: Wire up command dispatch when command system is implemented.
    pub fn on_download_finished(&self) {
        // C++: no-op for StreamCheckIntegrityEntry
        tracing::trace!("StreamCheckIntegrity::on_download_finished (no-op)");
    }

    /// Called when the download is incomplete after integrity check.
    ///
    /// In C++ `StreamCheckIntegrityEntry::onDownloadIncomplete()`:
    /// 1. `pieceStorage->onDownloadIncomplete()`
    /// 2. If `hash_check_only` is false, proceed to file allocation.
    ///
    /// # TODO
    ///
    /// - Wire up `PieceStorage::onDownloadIncomplete()` via registry.
    /// - Wire up file allocation entry creation and dispatch.
    pub fn on_download_incomplete(&self) {
        tracing::trace!(
            hash_check_only = self.hash_check_only,
            "StreamCheckIntegrity::on_download_incomplete"
        );

        // C++: ps->onDownloadIncomplete()
        // TODO: pieceStorage.on_download_incomplete() via registry.

        if self.hash_check_only {
            tracing::trace!("hash_check_only is set — skipping file allocation");
            return;
        }

        // C++: proceedFileAllocation(commands, StreamFileAllocationEntry, e)
        // TODO: Create StreamFileAllocationEntry and dispatch to FileAllocationMan.
    }

    /// Cut trailing garbage data beyond the expected total length.
    ///
    /// In C++, this calls `pieceStorage->getDiskAdaptor()->cutTrailingGarbage()`.
    // TODO: Wire up DiskAdaptor::cutTrailingGarbage() via registry.
    pub fn cut_trailing_garbage(&self) {
        tracing::trace!("StreamCheckIntegrity::cut_trailing_garbage");
        // TODO: Resolve PieceStorage from registry, then call disk_adaptor.cut_trailing_garbage().
    }

    /// Whether incomplete validation should be reported as an error.
    ///
    /// Matches C++ `CheckIntegrityEntry::shouldReportIncompleteAsError()`.
    /// Default is `true` for stream downloads.
    pub fn should_report_incomplete_as_error(&self) -> bool {
        true
    }

    /// Return whether hash-check-only mode is enabled.
    pub fn hash_check_only(&self) -> bool {
        self.hash_check_only
    }

    /// Set the hash-check-only flag.
    pub fn set_hash_check_only(&mut self, value: bool) {
        self.hash_check_only = value;
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
