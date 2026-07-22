//! Integrity checking pipeline for verifying downloaded data against known hashes.
//!
//! This module provides the infrastructure for verifying that downloaded file data
//! matches expected hash values. It replaces the C++ class hierarchy:
//!
//! ```text
//! C++ hierarchy                     Rust enum dispatch
//! ──────────────                    ──────────────────
//! IteratableValidator (virtual)  →  ValidatorKind (enum)
//!   └─ IteratableChunkChecksum      └─ ValidatorKind::PieceHash(PieceHashValidator)
//! CheckIntegrityEntry (virtual)  →  CheckIntegrityKind (enum)
//!   └─ PieceHashCheckIntegrityEntry  └─ CheckIntegrityKind::Stream / Bt
//!     └─ StreamCheckIntegrityEntry
//!     └─ BtCheckIntegrityEntry (implied)
//! ```
//!
//! # Design Rationale
//!
//! The C++ version uses deep inheritance + virtual dispatch for both the validator
//! and the entry hierarchy. Rust enum dispatch replaces both:
//!
//! - **No vtable overhead**: enum dispatch is zero-cost at the call site.
//! - **Exhaustive matching**: Adding a new variant requires updating all match arms,
//!   preventing the "forgot to override" bugs common in C++ virtual hierarchies.
//! - **Direct Arc<> references**: Uses `Arc<DownloadContext>` and `Arc<dyn PieceStorage>`
//!   instead of ID-based lookups, matching C++ `shared_ptr<>` semantics.
//!
//! # Interior Mutability Note
//!
//! `PieceStorage` trait methods like `mark_piece_verified` and `mark_piece_failed`
//! require `&mut self`, but the validator holds `Arc<dyn PieceStorage>` (shared
//! reference). To resolve this, `PieceHashValidator::validate_chunk()` collects
//! validation results internally, and the caller applies them via
//! `apply_validation_results()` when a `&mut dyn PieceStorage` is available.
//! This matches the C++ `shared_ptr<PieceStorage>` pattern where mutable access
//! is taken without Rust's borrow-checker enforcement.
//!
//! # C++ Reference
//!
//! - `IteratableValidator.h` — abstract async chunk-by-chunk validator interface
//! - `IteratableChunkChecksumValidator.h/.cc` — piece-hash based validator
//! - `CheckIntegrityEntry.h/.cc` — base entry for integrity checking operations
//! - `PieceHashCheckIntegrityEntry.h/.cc` — piece-hash based integrity checking
//! - `StreamCheckIntegrityEntry.h/.cc` — stream download integrity checking

use std::sync::Arc;
use tracing::{info, trace, warn};

// ---------------------------------------------------------------------------
// PieceValidationResult — per-piece validation outcome
// ---------------------------------------------------------------------------

/// Result of validating a single piece against its expected hash.
///
/// Collected by `PieceHashValidator::validate_chunk()` and later applied
/// to `PieceStorage` via `apply_validation_results()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceValidationResult {
    /// Piece hash matched the expected value.
    Verified {
        /// Index of the verified piece.
        piece_index: usize,
    },
    /// Piece hash did NOT match the expected value.
    Failed {
        /// Index of the failed piece.
        piece_index: usize,
    },
}

// ---------------------------------------------------------------------------
// ValidatorKind — replaces C++ IteratableValidator virtual hierarchy
// ---------------------------------------------------------------------------

/// Validator kind enum — replaces the C++ `IteratableValidator` virtual hierarchy.
///
/// Each variant carries its own state. The `None` variant represents the absence
/// of a validator (equivalent to a null `unique_ptr<IteratableValidator>` in C++).
///
/// # Lifecycle
///
/// 1. Start as `ValidatorKind::None` (no validator assigned).
/// 2. `init_validator()` creates the appropriate variant based on download type.
/// 3. Call `validate_chunk()` repeatedly until `is_finished()` returns `true`.
/// 4. Progress is available via `current_offset()` and `total_length()`.
#[derive(Debug)]
pub enum ValidatorKind {
    /// No validator assigned yet (equivalent to null `unique_ptr<IteratableValidator>`).
    None,
    /// Piece hash chunk checksum validator — validates each piece against its
    /// expected hash from the DownloadContext's piece hash list.
    /// Equivalent to C++ `IteratableChunkChecksumValidator`.
    PieceHash(PieceHashValidator),
    // Future variants can be added here:
    // /// Whole-file checksum validator
    // WholeHash(WholeHashValidator),
}

impl ValidatorKind {
    /// Initialize the validator for chunk-by-chunk processing.
    ///
    /// Delegates to the underlying validator's `init()` method.
    /// No-op for `ValidatorKind::None`.
    pub fn init(&mut self) {
        match self {
            ValidatorKind::None => {
                trace!("ValidatorKind::init called on None variant — no-op");
            }
            ValidatorKind::PieceHash(v) => v.init(),
        }
    }

    /// Validate a single chunk.
    ///
    /// Repeatedly calling this advances the validator through each piece.
    /// No-op for `ValidatorKind::None`.
    pub fn validate_chunk(&mut self) {
        match self {
            ValidatorKind::None => {
                trace!("ValidatorKind::validate_chunk called on None variant — no-op");
            }
            ValidatorKind::PieceHash(v) => v.validate_chunk(),
        }
    }

    /// Whether validation has completed all chunks.
    ///
    /// Returns `true` for `ValidatorKind::None` (nothing to validate = already done).
    pub fn is_finished(&self) -> bool {
        match self {
            ValidatorKind::None => true,
            ValidatorKind::PieceHash(v) => v.is_finished(),
        }
    }

    /// Current byte offset of the validation progress.
    ///
    /// Returns 0 for `ValidatorKind::None`.
    pub fn current_offset(&self) -> u64 {
        match self {
            ValidatorKind::None => 0,
            ValidatorKind::PieceHash(v) => v.current_offset(),
        }
    }

    /// Total byte length of the data being validated.
    ///
    /// Returns 0 for `ValidatorKind::None`.
    pub fn total_length(&self) -> u64 {
        match self {
            ValidatorKind::None => 0,
            ValidatorKind::PieceHash(v) => v.total_length(),
        }
    }

    /// Apply all collected validation results to the given PieceStorage.
    ///
    /// No-op for `ValidatorKind::None`.
    pub fn apply_validation_results(&self, ps: &mut dyn crate::segment::piece_storage::PieceStorage) {
        match self {
            ValidatorKind::None => {}
            ValidatorKind::PieceHash(v) => v.apply_validation_results(ps),
        }
    }
}

// ---------------------------------------------------------------------------
// PieceHashValidator — replaces C++ IteratableChunkChecksumValidator
// ---------------------------------------------------------------------------

/// Piece-hash based chunk checksum validator.
///
/// Validates each piece of a downloaded file against the expected piece hashes
/// stored in the `DownloadContext`. This is the Rust equivalent of the C++
/// `IteratableChunkChecksumValidator`.
///
/// # Algorithm (matching C++ behavior)
///
/// 1. `init()` — reset state, compute the expected bitfield length.
/// 2. `validate_chunk()` — read one piece from disk via PieceStorage, compute its
///    hash, compare against the expected hash from DownloadContext. Store the
///    result internally (does NOT directly mutate PieceStorage).
/// 3. When all pieces are validated, call `apply_validation_results()` to sync
///    the bitfield back to PieceStorage.
///
/// # Interior Mutability
///
/// `PieceStorage::mark_piece_verified()` and `mark_piece_failed()` require
/// `&mut self`, but this validator holds `Arc<dyn PieceStorage>` (shared ref).
/// Rather than wrapping the Arc in a Mutex (which adds overhead and complicates
/// the API), `validate_chunk()` collects results in a `Vec<PieceValidationResult>`.
/// The caller applies them via `apply_validation_results(&mut dyn PieceStorage)`.
///
/// # Direct Arc<> References
///
/// Holds `Arc<DownloadContext>` and `Arc<dyn PieceStorage>` directly, matching
/// C++ `shared_ptr<DownloadContext>` and `shared_ptr<PieceStorage>`. This
/// replaces the previous ID-based reference pattern.
pub struct PieceHashValidator {
    /// Shared download context containing piece hashes and metadata.
    /// C++ uses `shared_ptr<DownloadContext>`.
    download_context: Arc<crate::download::DownloadContext>,
    /// Shared piece storage holding the download bitfield and disk adaptor.
    /// C++ uses `shared_ptr<PieceStorage>`.
    /// Used for `read_data()` (`&self` method). Mutation is done via
    /// `apply_validation_results()` which takes `&mut dyn PieceStorage`.
    piece_storage: Arc<dyn crate::segment::piece_storage::PieceStorage>,
    /// Index of the piece currently being validated.
    current_piece_index: usize,
    /// Total number of pieces to validate.
    total_pieces: usize,
    /// Whether validation has completed all pieces.
    finished: bool,
    /// Byte offset of the current piece being validated.
    current_offset: u64,
    /// Total byte length of the data being validated.
    total_length: u64,
    /// Piece length in bytes (typically 1 MiB for HTTP, variable for BT).
    piece_length: u64,
    /// Number of pieces that passed hash verification.
    pieces_ok: usize,
    /// Number of pieces that failed hash verification.
    pieces_failed: usize,
    /// Collected validation results, applied later via `apply_validation_results()`.
    validation_results: Vec<PieceValidationResult>,
}

impl std::fmt::Debug for PieceHashValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PieceHashValidator")
            .field("current_piece_index", &self.current_piece_index)
            .field("total_pieces", &self.total_pieces)
            .field("finished", &self.finished)
            .field("current_offset", &self.current_offset)
            .field("total_length", &self.total_length)
            .field("piece_length", &self.piece_length)
            .field("pieces_ok", &self.pieces_ok)
            .field("pieces_failed", &self.pieces_failed)
            .field("validation_results_len", &self.validation_results.len())
            .finish()
    }
}

impl PieceHashValidator {
    /// Create a new `PieceHashValidator`.
    ///
    /// # Arguments
    ///
    /// * `download_context` — Shared DownloadContext containing piece hashes.
    /// * `piece_storage` — Shared PieceStorage holding the download bitfield.
    /// * `total_pieces` — Total number of pieces to validate.
    /// * `total_length` — Total byte length of the download.
    /// * `piece_length` — Byte length of each piece (except possibly the last).
    pub fn new(
        download_context: Arc<crate::download::DownloadContext>,
        piece_storage: Arc<dyn crate::segment::piece_storage::PieceStorage>,
        total_pieces: usize,
        total_length: u64,
        piece_length: u64,
    ) -> Self {
        Self {
            download_context,
            piece_storage,
            current_piece_index: 0,
            total_pieces,
            finished: total_pieces == 0,
            current_offset: 0,
            total_length,
            piece_length,
            pieces_ok: 0,
            pieces_failed: 0,
            validation_results: Vec::new(),
        }
    }

    /// Initialize the validator for a fresh validation pass.
    ///
    /// Resets piece index, offset, and finished flag. In the C++ version, this
    /// also creates the `MessageDigest` context and clears the bitfield.
    pub fn init(&mut self) {
        trace!(
            total_pieces = self.total_pieces,
            "PieceHashValidator initializing"
        );
        self.current_piece_index = 0;
        self.current_offset = 0;
        self.finished = self.total_pieces == 0;
        self.pieces_ok = 0;
        self.pieces_failed = 0;
        self.validation_results.clear();
    }

    /// Validate a single piece (chunk).
    ///
    /// Reads the piece data from disk via PieceStorage, computes its hash,
    /// and compares against the expected hash from DownloadContext. The result
    /// is stored internally in `validation_results` — it is NOT applied to
    /// PieceStorage directly because `mark_piece_verified`/`mark_piece_failed`
    /// require `&mut self` and we hold `Arc<dyn PieceStorage>`.
    ///
    /// Call `apply_validation_results()` after validation completes to apply
    /// all results to a `&mut dyn PieceStorage`.
    ///
    /// In the C++ version (`IteratableChunkChecksumValidator::validateChunk()`):
    /// 1. Computes the expected piece length (last piece may be shorter).
    /// 2. Reads piece data from DiskAdaptor.
    /// 3. Hashes the data and compares with the expected piece hash.
    /// 4. Sets/unsets the bitfield bit for this piece.
    /// 5. Advances to the next piece index.
    /// 6. When finished, syncs the bitfield back to PieceStorage.
    pub fn validate_chunk(&mut self) {
        if self.finished {
            trace!("PieceHashValidator::validate_chunk called after completion — no-op");
            return;
        }

        let piece_index = self.current_piece_index;

        // Determine piece length (last piece may be shorter).
        let piece_len = if piece_index + 1 == self.total_pieces {
            self.total_length.saturating_sub(self.current_offset)
        } else {
            self.piece_length
        };

        // Compute expected hash for this piece from DownloadContext.
        // get_piece_hash() returns "" if index is out of range.
        let expected_hash = self.download_context.get_piece_hash(piece_index);
        let has_expected = !expected_hash.is_empty();

        // Read piece data from DiskAdaptor via PieceStorage.
        // In C++, this calls `pieceStorage_->getDiskAdaptor()->readData()`.
        // Here we use the PieceStorage's read interface.
        match self.piece_storage.read_data(piece_index) {
            Ok(data) if data.len() as u64 == piece_len => {
                // Compute the hash of the piece data.
                let computed_hash = self.compute_hash(&data);

                if has_expected {
                    if computed_hash.eq_ignore_ascii_case(expected_hash) {
                        // Hash matches — record verified result.
                        trace!(piece_index, "Piece hash verified OK");
                        self.validation_results
                            .push(PieceValidationResult::Verified { piece_index });
                        self.pieces_ok += 1;
                    } else {
                        // Hash mismatch — record failed result.
                        warn!(
                            piece_index,
                            expected_hash_len = expected_hash.len(),
                            computed_hash_len = computed_hash.len(),
                            "Piece hash mismatch — marking for re-download"
                        );
                        self.validation_results
                            .push(PieceValidationResult::Failed { piece_index });
                        self.pieces_failed += 1;
                    }
                } else {
                    // No expected hash available — cannot verify this piece.
                    // In C++ this doesn't happen (pieces always have hashes),
                    // but we handle it gracefully.
                    trace!(piece_index, "No expected hash for piece — skipping verification");
                }
            }
            Ok(data) => {
                // Read returned wrong length — treat as failure.
                warn!(
                    piece_index,
                    expected_len = piece_len,
                    actual_len = data.len(),
                    "Piece data length mismatch during integrity check"
                );
                self.validation_results
                    .push(PieceValidationResult::Failed { piece_index });
                self.pieces_failed += 1;
            }
            Err(e) => {
                // I/O error reading piece data — treat as failure.
                // In C++, `RecoverableException` causes the bit to be unset.
                warn!(
                    piece_index,
                    error = %e,
                    "Failed to read piece data during integrity check"
                );
                self.validation_results
                    .push(PieceValidationResult::Failed { piece_index });
                self.pieces_failed += 1;
            }
        }

        // Advance to next piece
        self.current_piece_index += 1;
        self.current_offset = self.current_piece_index as u64 * self.piece_length;

        // Cap offset at total length
        if self.current_offset > self.total_length {
            self.current_offset = self.total_length;
        }

        // Check if we've validated all pieces
        if self.current_piece_index >= self.total_pieces {
            self.finished = true;
            info!(
                total_pieces = self.total_pieces,
                pieces_ok = self.pieces_ok,
                pieces_failed = self.pieces_failed,
                "PieceHashValidator completed all piece validation"
            );
        }
    }

    /// Apply all collected validation results to the given PieceStorage.
    ///
    /// This is the Rust equivalent of the C++ pattern where
    /// `IteratableChunkChecksumValidator` directly calls
    /// `pieceStorage_->setBit(index)` or `unsetBit(index)` on the bitfield.
    /// In Rust, we separate validation from mutation because `Arc<dyn PieceStorage>`
    /// does not allow calling `&mut self` methods.
    ///
    /// The caller must provide a `&mut dyn PieceStorage` to apply results to.
    /// This is typically done after `is_finished()` returns `true`.
    pub fn apply_validation_results(
        &self,
        ps: &mut dyn crate::segment::piece_storage::PieceStorage,
    ) {
        for result in &self.validation_results {
            match result {
                PieceValidationResult::Verified { piece_index } => {
                    ps.mark_piece_verified(*piece_index);
                }
                PieceValidationResult::Failed { piece_index } => {
                    ps.mark_piece_failed(*piece_index);
                }
            }
        }
    }

    /// Compute the SHA-1 hash of a piece's data and return as hex string.
    ///
    /// In C++, this uses `MessageDigest` with the algorithm from
    /// `dctx->getPieceHashType()`. Currently only SHA-1 is supported
    /// (the standard for BitTorrent and most HTTP downloads).
    fn compute_hash(&self, data: &[u8]) -> String {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(data);
        let result = hasher.finalize();
        hex::encode(result)
    }

    /// Whether validation has completed all pieces.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Current byte offset of the validation progress.
    ///
    /// Matches C++ `getCurrentOffset()`: returns `currentIndex * pieceLength`.
    pub fn current_offset(&self) -> u64 {
        self.current_offset
    }

    /// Total byte length of the data being validated.
    ///
    /// Matches C++ `getTotalLength()`: returns the total download length.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Return the current piece index being validated.
    pub fn current_piece_index(&self) -> usize {
        self.current_piece_index
    }

    /// Return the total number of pieces.
    pub fn total_pieces(&self) -> usize {
        self.total_pieces
    }

    /// Return the number of pieces that passed verification.
    pub fn pieces_ok(&self) -> usize {
        self.pieces_ok
    }

    /// Return the number of pieces that failed verification.
    pub fn pieces_failed(&self) -> usize {
        self.pieces_failed
    }

    /// Return a reference to the collected validation results.
    pub fn validation_results(&self) -> &[PieceValidationResult] {
        &self.validation_results
    }

    /// Return a reference to the DownloadContext.
    pub fn download_context(&self) -> &Arc<crate::download::DownloadContext> {
        &self.download_context
    }

    /// Return a reference to the PieceStorage.
    pub fn piece_storage(&self) -> &Arc<dyn crate::segment::piece_storage::PieceStorage> {
        &self.piece_storage
    }
}

// ---------------------------------------------------------------------------
// StreamCheckIntegrity — replaces C++ StreamCheckIntegrityEntry
// ---------------------------------------------------------------------------

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
        trace!("StreamCheckIntegrity initializing validator");

        if let (Some(ctx), Some(ps)) = (&self.download_context, &self.piece_storage) {
            let total_pieces = ctx.get_piece_hashes().len();
            if total_pieces == 0 {
                trace!("No piece hashes available — skipping validator creation");
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
        trace!("StreamCheckIntegrity::on_download_finished (no-op)");
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
        trace!(
            hash_check_only = self.hash_check_only,
            "StreamCheckIntegrity::on_download_incomplete"
        );

        // C++: ps->onDownloadIncomplete()
        // TODO: pieceStorage.on_download_incomplete() via registry.

        if self.hash_check_only {
            trace!("hash_check_only is set — skipping file allocation");
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
        trace!("StreamCheckIntegrity::cut_trailing_garbage");
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

// ---------------------------------------------------------------------------
// BtCheckIntegrity — replaces C++ BtCheckIntegrityEntry (implied)
// ---------------------------------------------------------------------------

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
        trace!("BtCheckIntegrity initializing validator");

        if let (Some(ctx), Some(ps)) = (&self.download_context, &self.piece_storage) {
            let total_pieces = ctx.get_piece_hashes().len();
            if total_pieces == 0 {
                trace!("No piece hashes available — skipping validator creation");
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
        trace!("BtCheckIntegrity::on_download_finished (no-op)");
    }

    /// Called when the download is incomplete after integrity check.
    ///
    /// For BT downloads, this signals that some pieces failed verification.
    /// Unlike stream downloads, BT does NOT proceed to file allocation —
    /// the BT pipeline re-downloads missing pieces through its own mechanism.
    // TODO: Wire up PieceStorage::onDownloadIncomplete() via registry.
    pub fn on_download_incomplete(&self) {
        trace!("BtCheckIntegrity::on_download_incomplete");
        // C++: ps->onDownloadIncomplete()
        // No file allocation for BT downloads.
    }

    /// Cut trailing garbage data beyond the expected total length.
    // TODO: Wire up DiskAdaptor::cutTrailingGarbage() via registry.
    pub fn cut_trailing_garbage(&self) {
        trace!("BtCheckIntegrity::cut_trailing_garbage");
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

// ---------------------------------------------------------------------------
// CheckIntegrityKind — replaces C++ CheckIntegrityEntry hierarchy
// ---------------------------------------------------------------------------

/// CheckIntegrity entry kind — replaces the C++ `CheckIntegrityEntry` hierarchy.
///
/// Uses enum dispatch instead of virtual inheritance to provide zero-cost
/// polymorphism. Each variant carries its own complete state.
///
/// # C++ Mapping
///
/// | C++ Class                     | Rust Variant                      |
/// |-------------------------------|-----------------------------------|
/// | `StreamCheckIntegrityEntry`   | `CheckIntegrityKind::Stream`      |
/// | (implied) BtCheckIntegrityEntry | `CheckIntegrityKind::Bt`       |
#[derive(Debug)]
pub enum CheckIntegrityKind {
    /// Stream (HTTP/FTP) download integrity check.
    Stream(StreamCheckIntegrity),
    /// BitTorrent download integrity check.
    Bt(BtCheckIntegrity),
}

impl CheckIntegrityKind {
    /// Whether the validation is ready to begin.
    pub fn is_validation_ready(&self) -> bool {
        match self {
            CheckIntegrityKind::Stream(s) => s.is_validation_ready(),
            CheckIntegrityKind::Bt(b) => b.is_validation_ready(),
        }
    }

    /// Initialize the validator.
    ///
    /// Each variant retrieves its metadata from the stored `Arc<DownloadContext>`,
    /// so no external parameters are needed.
    pub fn init_validator(&mut self) {
        match self {
            CheckIntegrityKind::Stream(s) => s.init_validator(),
            CheckIntegrityKind::Bt(b) => b.init_validator(),
        }
    }

    /// Validate a single chunk.
    pub fn validate_chunk(&mut self) {
        match self {
            CheckIntegrityKind::Stream(s) => s.validate_chunk(),
            CheckIntegrityKind::Bt(b) => b.validate_chunk(),
        }
    }

    /// Whether the integrity check has completed.
    pub fn is_finished(&self) -> bool {
        match self {
            CheckIntegrityKind::Stream(s) => s.is_finished(),
            CheckIntegrityKind::Bt(b) => b.is_finished(),
        }
    }

    /// Total byte length of the data being validated.
    pub fn total_length(&self) -> u64 {
        match self {
            CheckIntegrityKind::Stream(s) => s.total_length(),
            CheckIntegrityKind::Bt(b) => b.total_length(),
        }
    }

    /// Current validated byte length.
    pub fn current_length(&self) -> u64 {
        match self {
            CheckIntegrityKind::Stream(s) => s.current_length(),
            CheckIntegrityKind::Bt(b) => b.current_length(),
        }
    }

    /// Called when the download finishes successfully after integrity check.
    pub fn on_download_finished(&self) {
        match self {
            CheckIntegrityKind::Stream(s) => s.on_download_finished(),
            CheckIntegrityKind::Bt(b) => b.on_download_finished(),
        }
    }

    /// Called when the download is incomplete after integrity check.
    pub fn on_download_incomplete(&self) {
        match self {
            CheckIntegrityKind::Stream(s) => s.on_download_incomplete(),
            CheckIntegrityKind::Bt(b) => b.on_download_incomplete(),
        }
    }

    /// Cut trailing garbage data beyond the expected total length.
    pub fn cut_trailing_garbage(&self) {
        match self {
            CheckIntegrityKind::Stream(s) => s.cut_trailing_garbage(),
            CheckIntegrityKind::Bt(b) => b.cut_trailing_garbage(),
        }
    }

    /// Whether incomplete validation should be reported as an error.
    pub fn should_report_incomplete_as_error(&self) -> bool {
        match self {
            CheckIntegrityKind::Stream(s) => s.should_report_incomplete_as_error(),
            CheckIntegrityKind::Bt(b) => b.should_report_incomplete_as_error(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test fixture helpers ─────────────────────────────────────────────────

    /// Create a test `Arc<DownloadContext>` with the given piece length and total length.
    fn make_dctx(piece_length: u32, total_length: u64) -> Arc<crate::download::DownloadContext> {
        Arc::new(crate::download::DownloadContext::new(
            piece_length,
            total_length,
            "/tmp/test_check_integrity.bin".to_string(),
        ))
    }

    /// Create a test `Arc<dyn PieceStorage>` with the given piece length and total length.
    fn make_ps(
        piece_length: u64,
        total_length: u64,
    ) -> Arc<dyn crate::segment::piece_storage::PieceStorage> {
        Arc::new(crate::segment::piece_storage::DefaultPieceStorage::new(
            piece_length,
            total_length,
        ))
    }

    // ── ValidatorKind enum dispatch tests ─────────────────────────────────

    #[test]
    fn test_validator_kind_none_is_finished() {
        let v = ValidatorKind::None;
        assert!(v.is_finished(), "None variant should be finished (nothing to validate)");
    }

    #[test]
    fn test_validator_kind_none_zero_metrics() {
        let v = ValidatorKind::None;
        assert_eq!(v.current_offset(), 0);
        assert_eq!(v.total_length(), 0);
    }

    #[test]
    fn test_validator_kind_none_init_and_validate_noop() {
        let mut v = ValidatorKind::None;
        // Should not panic or change state
        v.init();
        v.validate_chunk();
        assert!(v.is_finished());
    }

    #[test]
    fn test_validator_kind_piece_hash_dispatch() {
        let ctx = make_dctx(1_048_576, 5_242_880);
        let ps = make_ps(1_048_576, 5_242_880);
        let v = ValidatorKind::PieceHash(
            PieceHashValidator::new(ctx, ps, 5, 5_242_880, 1_048_576)
        );
        assert!(!v.is_finished(), "PieceHash with 5 pieces should not be finished initially");
        assert_eq!(v.total_length(), 5_242_880);
        assert_eq!(v.current_offset(), 0);
    }

    // ── PieceHashValidator init and state tracking tests ──────────────────

    #[test]
    fn test_piece_hash_validator_new() {
        let ctx = make_dctx(1_048_576, 4_194_304);
        let ps = make_ps(1_048_576, 4_194_304);
        let v = PieceHashValidator::new(ctx, ps, 4, 4_194_304, 1_048_576);
        assert_eq!(v.current_piece_index(), 0);
        assert_eq!(v.total_pieces(), 4);
        assert!(!v.is_finished());
        assert_eq!(v.current_offset(), 0);
        assert_eq!(v.total_length(), 4_194_304);
    }

    #[test]
    fn test_piece_hash_validator_zero_pieces_is_finished() {
        let ctx = make_dctx(1_048_576, 0);
        let ps = make_ps(1_048_576, 0);
        let v = PieceHashValidator::new(ctx, ps, 0, 0, 1_048_576);
        assert!(v.is_finished(), "Zero pieces should be immediately finished");
    }

    #[test]
    fn test_piece_hash_validator_init_resets_state() {
        let ctx = make_dctx(1_048_576, 3_145_728);
        let ps = make_ps(1_048_576, 3_145_728);
        let mut v = PieceHashValidator::new(ctx, ps, 3, 3_145_728, 1_048_576);
        // Simulate partial progress
        v.validate_chunk();
        v.validate_chunk();
        assert_eq!(v.current_piece_index(), 2);

        // Init should reset
        v.init();
        assert_eq!(v.current_piece_index(), 0);
        assert_eq!(v.current_offset(), 0);
        assert!(!v.is_finished());
    }

    #[test]
    fn test_piece_hash_validator_init_with_zero_pieces() {
        let ctx = make_dctx(1024, 0);
        let ps = make_ps(1024, 0);
        let mut v = PieceHashValidator::new(ctx, ps, 0, 0, 1024);
        v.init();
        assert!(v.is_finished(), "Init with zero pieces should set finished");
    }

    // ── Saturated validation progress tests ───────────────────────────────

    #[test]
    fn test_piece_hash_validator_validate_chunk_advances() {
        let ctx = make_dctx(1_048_576, 3_145_728);
        let ps = make_ps(1_048_576, 3_145_728);
        let mut v = PieceHashValidator::new(ctx, ps, 3, 3_145_728, 1_048_576);

        v.validate_chunk();
        assert_eq!(v.current_piece_index(), 1);
        assert_eq!(v.current_offset(), 1_048_576);
        assert!(!v.is_finished());

        v.validate_chunk();
        assert_eq!(v.current_piece_index(), 2);
        assert_eq!(v.current_offset(), 2_097_152);
        assert!(!v.is_finished());
    }

    #[test]
    fn test_piece_hash_validator_saturates_at_total_length() {
        let ctx = make_dctx(1_048_576, 2_097_152);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

        v.validate_chunk(); // piece 0 → piece 1
        v.validate_chunk(); // piece 1 → finished

        assert!(v.is_finished());
        // After finishing, offset should not exceed total_length
        assert!(v.current_offset() <= v.total_length());
    }

    // ── Finished flag management tests ────────────────────────────────────

    #[test]
    fn test_piece_hash_validator_finished_after_all_chunks() {
        let ctx = make_dctx(1_048_576, 2_097_152);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

        assert!(!v.is_finished());
        v.validate_chunk();
        assert!(!v.is_finished());
        v.validate_chunk();
        assert!(v.is_finished());
    }

    #[test]
    fn test_piece_hash_validator_validate_after_finished_is_noop() {
        let ctx = make_dctx(1_048_576, 1_048_576);
        let ps = make_ps(1_048_576, 1_048_576);
        let mut v = PieceHashValidator::new(ctx, ps, 1, 1_048_576, 1_048_576);

        v.validate_chunk();
        assert!(v.is_finished());

        // Calling validate_chunk again should not panic or change state
        v.validate_chunk();
        assert!(v.is_finished());
        assert_eq!(v.current_piece_index(), 1);
    }

    // ── Validation result collection tests ─────────────────────────────────

    #[test]
    fn test_piece_hash_validator_collects_failed_results() {
        // No disk adaptor connected → all reads fail → all pieces marked failed
        let ctx = make_dctx(1_048_576, 2_097_152);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

        v.validate_chunk();
        v.validate_chunk();
        assert!(v.is_finished());

        // All pieces should have failed (no disk adaptor)
        let results = v.validation_results();
        assert_eq!(results.len(), 2);
        assert!(matches!(results[0], PieceValidationResult::Failed { piece_index: 0 }));
        assert!(matches!(results[1], PieceValidationResult::Failed { piece_index: 1 }));
        assert_eq!(v.pieces_failed(), 2);
        assert_eq!(v.pieces_ok(), 0);
    }

    #[test]
    fn test_piece_hash_validator_apply_results() {
        let ctx = make_dctx(1_048_576, 2_097_152);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

        v.validate_chunk();
        v.validate_chunk();

        // Apply results to a fresh PieceStorage
        let mut ps2 = crate::segment::piece_storage::DefaultPieceStorage::new(1_048_576, 2_097_152);
        v.apply_validation_results(&mut ps2);
        // Should not panic
    }

    // ── CheckIntegrityKind enum dispatch tests ────────────────────────────

    #[test]
    fn test_check_integrity_kind_stream() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let entry = CheckIntegrityKind::Stream(
            StreamCheckIntegrity::new(ctx, ps, false)
        );
        assert!(!entry.is_validation_ready()); // No piece hashes set
        assert!(entry.is_finished()); // No validator yet (None), so finished
        assert_eq!(entry.total_length(), 0);
        assert_eq!(entry.current_length(), 0);
        assert!(entry.should_report_incomplete_as_error());
    }

    #[test]
    fn test_check_integrity_kind_bt() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let entry = CheckIntegrityKind::Bt(
            BtCheckIntegrity::new(ctx, ps)
        );
        assert!(!entry.is_validation_ready()); // No piece hashes set
        assert!(entry.is_finished()); // No validator yet (None), so finished
        assert_eq!(entry.total_length(), 0);
        assert_eq!(entry.current_length(), 0);
        assert!(!entry.should_report_incomplete_as_error());
    }

    #[test]
    fn test_check_integrity_kind_init_and_validate_stream() {
        let ctx = make_dctx(1_048_576, 3_145_728);
        let ps = make_ps(1_048_576, 3_145_728);
        let mut entry = CheckIntegrityKind::Stream(
            StreamCheckIntegrity::new(ctx, ps, false)
        );
        entry.init_validator();
        // Without piece hashes, the validator won't be created,
        // so it remains finished (ValidatorKind::None).
        assert!(entry.is_finished());
    }

    #[test]
    fn test_check_integrity_kind_init_and_validate_bt() {
        let ctx = make_dctx(1_048_576, 2_097_152);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut entry = CheckIntegrityKind::Bt(
            BtCheckIntegrity::new(ctx, ps)
        );
        entry.init_validator();
        // Without piece hashes, the validator won't be created,
        // so it remains finished (ValidatorKind::None).
        assert!(entry.is_finished());
    }

    // ── StreamCheckIntegrity creation and validation_ready tests ──────────

    #[test]
    fn test_stream_check_integrity_new() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let s = StreamCheckIntegrity::new(ctx, ps, false);
        assert!(!s.hash_check_only());
        assert!(s.is_finished()); // No validator → finished
    }

    #[test]
    fn test_stream_check_integrity_hash_check_only() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let mut s = StreamCheckIntegrity::new(ctx, ps, true);
        assert!(s.hash_check_only());
        s.set_hash_check_only(false);
        assert!(!s.hash_check_only());
    }

    #[test]
    fn test_stream_check_integrity_validation_ready() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let s = StreamCheckIntegrity::new(ctx, ps, false);
        // No piece hashes set → not ready
        assert!(!s.is_validation_ready());
    }

    #[test]
    fn test_stream_check_integrity_validation_ready_with_hashes() {
        let mut ctx = crate::download::DownloadContext::new(1024, 4096, "/tmp/test.bin".to_string());
        ctx.set_piece_hashes(
            "sha-1".to_string(),
            vec!["h1".to_string(), "h2".to_string(), "h3".to_string(), "h4".to_string()],
        );
        let ctx = Arc::new(ctx);
        let ps = make_ps(1024, 4096);
        let s = StreamCheckIntegrity::new(ctx, ps, false);
        assert!(s.is_validation_ready());
    }

    #[test]
    fn test_stream_check_integrity_init_validator() {
        let ctx = make_dctx(1_048_576, 4_194_304);
        let ps = make_ps(1_048_576, 4_194_304);
        let mut s = StreamCheckIntegrity::new(ctx, ps, false);
        assert!(s.is_finished()); // No validator yet

        // Without piece hashes, init_validator is a no-op (validator stays None)
        s.init_validator();
        assert!(s.is_finished()); // Still finished (no validator created)
    }

    #[test]
    fn test_stream_check_integrity_init_validator_with_hashes() {
        let mut ctx = crate::download::DownloadContext::new(1_048_576, 4_194_304, "/tmp/test.bin".to_string());
        ctx.set_piece_hashes(
            "sha-1".to_string(),
            vec!["h1".to_string(), "h2".to_string(), "h3".to_string(), "h4".to_string()],
        );
        let ctx = Arc::new(ctx);
        let ps = make_ps(1_048_576, 4_194_304);
        let mut s = StreamCheckIntegrity::new(ctx, ps, false);
        assert!(s.is_finished()); // No validator yet

        s.init_validator();
        assert!(!s.is_finished()); // Validator created, not yet finished
        assert_eq!(s.total_length(), 4_194_304);
    }

    #[test]
    fn test_stream_check_integrity_validator_access() {
        let ctx = make_dctx(1_048_576, 1_048_576);
        let ps = make_ps(1_048_576, 1_048_576);
        let mut s = StreamCheckIntegrity::new(ctx, ps, false);
        assert!(matches!(s.validator(), ValidatorKind::None));

        // Without piece hashes, init_validator is a no-op
        s.init_validator();
        assert!(matches!(s.validator(), ValidatorKind::None));
    }

    #[test]
    fn test_stream_check_integrity_on_download_finished_noop() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let s = StreamCheckIntegrity::new(ctx, ps, false);
        // Should not panic
        s.on_download_finished();
    }

    #[test]
    fn test_stream_check_integrity_on_download_incomplete() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let s = StreamCheckIntegrity::new(ctx, ps, false);
        // Should not panic
        s.on_download_incomplete();
    }

    #[test]
    fn test_stream_check_integrity_hash_check_only_skips_allocation() {
        // This test verifies the hash_check_only path logic.
        // The actual file allocation dispatch is TODO, but we verify
        // the method runs without panic for both branches.
        let ctx1 = make_dctx(1024, 4096);
        let ps1 = make_ps(1024, 4096);
        let ctx2 = make_dctx(1024, 4096);
        let ps2 = make_ps(1024, 4096);
        let s_with = StreamCheckIntegrity::new(ctx1, ps1, true);
        let s_without = StreamCheckIntegrity::new(ctx2, ps2, false);
        s_with.on_download_incomplete();
        s_without.on_download_incomplete();
    }

    // ── BtCheckIntegrity tests ────────────────────────────────────────────

    #[test]
    fn test_bt_check_integrity_new() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let b = BtCheckIntegrity::new(ctx, ps);
        assert!(b.is_finished()); // No validator yet
        assert!(!b.should_report_incomplete_as_error());
    }

    #[test]
    fn test_bt_check_integrity_init_validator() {
        let mut ctx = crate::download::DownloadContext::new(1_048_576, 2_097_152, "/tmp/test.bin".to_string());
        ctx.set_piece_hashes(
            "sha-1".to_string(),
            vec!["h1".to_string(), "h2".to_string()],
        );
        let ctx = Arc::new(ctx);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut b = BtCheckIntegrity::new(ctx, ps);
        b.init_validator();
        assert!(!b.is_finished());
        assert_eq!(b.total_length(), 2_097_152);
    }

    #[test]
    fn test_bt_check_integrity_on_download_handlers() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let b = BtCheckIntegrity::new(ctx, ps);
        // Should not panic
        b.on_download_finished();
        b.on_download_incomplete();
    }

    // ── Cross-cutting: ValidatorKind after PieceHashValidator assignment ───

    #[test]
    fn test_validator_kind_piece_hash_full_lifecycle() {
        let ctx = make_dctx(1_048_576, 2_097_152);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut v = ValidatorKind::PieceHash(
            PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576)
        );

        v.init();
        assert!(!v.is_finished());
        assert_eq!(v.current_offset(), 0);

        v.validate_chunk();
        assert_eq!(v.current_offset(), 1_048_576);

        v.validate_chunk();
        assert!(v.is_finished());
        assert_eq!(v.current_offset(), 2_097_152);

        // Validate after finish should be no-op
        v.validate_chunk();
        assert!(v.is_finished());
    }

    #[test]
    fn test_validator_kind_init_on_piece_hash() {
        let ctx = make_dctx(1_048_576, 3_145_728);
        let ps = make_ps(1_048_576, 3_145_728);
        let mut v = ValidatorKind::PieceHash(
            PieceHashValidator::new(ctx, ps, 3, 3_145_728, 1_048_576)
        );
        v.validate_chunk(); // advance to piece 1
        assert_eq!(v.current_offset(), 1_048_576);

        v.init(); // reset
        assert_eq!(v.current_offset(), 0);
        assert!(!v.is_finished());
    }

    #[test]
    fn test_validator_kind_apply_validation_results() {
        let ctx = make_dctx(1_048_576, 2_097_152);
        let ps = make_ps(1_048_576, 2_097_152);
        let mut v = ValidatorKind::PieceHash(
            PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576)
        );

        v.validate_chunk();
        v.validate_chunk();
        assert!(v.is_finished());

        // Apply results to a fresh PieceStorage
        let mut ps2 = crate::segment::piece_storage::DefaultPieceStorage::new(1_048_576, 2_097_152);
        v.apply_validation_results(&mut ps2);
        // Should not panic
    }

    #[test]
    fn test_validator_kind_apply_validation_results_none_variant() {
        let v = ValidatorKind::None;
        let mut ps = crate::segment::piece_storage::DefaultPieceStorage::new(1_048_576, 2_097_152);
        // Should not panic — no-op for None variant
        v.apply_validation_results(&mut ps);
    }

    // ── Debug trait tests ──────────────────────────────────────────────────

    #[test]
    fn test_stream_check_integrity_debug() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let s = StreamCheckIntegrity::new(ctx, ps, false);
        let debug_str = format!("{:?}", s);
        assert!(debug_str.contains("StreamCheckIntegrity"));
        assert!(debug_str.contains("hash_check_only: false"));
    }

    #[test]
    fn test_bt_check_integrity_debug() {
        let ctx = make_dctx(1024, 4096);
        let ps = make_ps(1024, 4096);
        let b = BtCheckIntegrity::new(ctx, ps);
        let debug_str = format!("{:?}", b);
        assert!(debug_str.contains("BtCheckIntegrity"));
    }

    #[test]
    fn test_piece_validation_result_debug() {
        let r1 = PieceValidationResult::Verified { piece_index: 0 };
        let r2 = PieceValidationResult::Failed { piece_index: 1 };
        assert!(format!("{:?}", r1).contains("Verified"));
        assert!(format!("{:?}", r2).contains("Failed"));
    }
}
