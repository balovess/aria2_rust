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
//! - **ID-based references**: Uses `u64` IDs instead of `Arc<>` or raw pointers
//!   to reference DownloadContext, PieceStorage, and RequestGroup. This matches
//!   the project's existing pattern and avoids lifetime complexity. Objects are
//!   resolved through a central registry at runtime.
//!
//! # C++ Reference
//!
//! - `IteratableValidator.h` — abstract async chunk-by-chunk validator interface
//! - `IteratableChunkChecksumValidator.h/.cc` — piece-hash based validator
//! - `CheckIntegrityEntry.h/.cc` — base entry for integrity checking operations
//! - `PieceHashCheckIntegrityEntry.h/.cc` — piece-hash based integrity checking
//! - `StreamCheckIntegrityEntry.h/.cc` — stream download integrity checking

use tracing::{info, trace};

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
/// 1. `init()` — reset state, prepare hash context for the piece hash algorithm.
/// 2. `validate_chunk()` — read one piece from disk, compute its hash, compare
///    against the expected hash. Update the bitfield accordingly.
/// 3. When all pieces are validated, sync the bitfield back to PieceStorage.
///
/// # ID-based References
///
/// Instead of holding `Arc<DownloadContext>` and `Arc<PieceStorage>` (as the C++
/// version does with `shared_ptr`), this struct uses `u64` IDs. The actual
/// objects are resolved through a central registry at runtime when I/O is
/// performed. This avoids lifetime complexity and reference cycles.
///
/// # TODO
///
/// - Wire up actual disk I/O through DiskAdaptor (currently skeleton).
/// - Implement bitfield update on validation completion.
/// - Implement piece hash lookup from DownloadContext registry.
#[derive(Debug)]
pub struct PieceHashValidator {
    /// ID reference to the DownloadContext that holds piece hashes and metadata.
    download_context_id: u64,
    /// ID reference to the PieceStorage that holds the download bitfield and disk adaptor.
    piece_storage_id: u64,
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
}

impl PieceHashValidator {
    /// Create a new `PieceHashValidator`.
    ///
    /// # Arguments
    ///
    /// * `download_context_id` — ID of the DownloadContext containing piece hashes.
    /// * `piece_storage_id` — ID of the PieceStorage holding the download bitfield.
    /// * `total_pieces` — Total number of pieces to validate.
    /// * `total_length` — Total byte length of the download.
    /// * `piece_length` — Byte length of each piece (except possibly the last).
    pub fn new(
        download_context_id: u64,
        piece_storage_id: u64,
        total_pieces: usize,
        total_length: u64,
        piece_length: u64,
    ) -> Self {
        Self {
            download_context_id,
            piece_storage_id,
            current_piece_index: 0,
            total_pieces,
            finished: total_pieces == 0,
            current_offset: 0,
            total_length,
            piece_length,
        }
    }

    /// Initialize the validator for a fresh validation pass.
    ///
    /// Resets piece index, offset, and finished flag. In the C++ version, this
    /// also creates the `MessageDigest` context and clears the bitfield.
    /// The actual I/O and hash context setup will be wired when DiskAdaptor
    /// integration is complete.
    pub fn init(&mut self) {
        trace!(
            dctx_id = self.download_context_id,
            ps_id = self.piece_storage_id,
            total_pieces = self.total_pieces,
            "PieceHashValidator initializing"
        );
        self.current_piece_index = 0;
        self.current_offset = 0;
        self.finished = self.total_pieces == 0;

        // TODO: Create MessageDigest context from DownloadContext's piece_hash_type.
        // TODO: Clear the bitfield in PieceStorage.
    }

    /// Validate a single piece (chunk).
    ///
    /// Reads the piece data from disk, computes its hash, and compares against
    /// the expected hash from DownloadContext. Updates the bitfield accordingly.
    ///
    /// In the C++ version, this method:
    /// 1. Computes the expected piece length (last piece may be shorter).
    /// 2. Reads piece data from DiskAdaptor.
    /// 3. Hashes the data and compares with the expected piece hash.
    /// 4. Sets/unsets the bitfield bit for this piece.
    /// 5. Advances to the next piece index.
    /// 6. When finished, syncs the bitfield back to PieceStorage.
    ///
    /// # TODO
    ///
    /// - Wire up actual disk read via DiskAdaptor.
    /// - Wire up piece hash lookup from DownloadContext.
    /// - Wire up bitfield update in PieceStorage.
    pub fn validate_chunk(&mut self) {
        if self.finished {
            trace!("PieceHashValidator::validate_chunk called after completion — no-op");
            return;
        }

        trace!(
            piece_index = self.current_piece_index,
            total_pieces = self.total_pieces,
            offset = self.current_offset,
            "Validating piece chunk"
        );

        // TODO: Actual hash validation logic:
        //   1. Determine piece length (last piece may be shorter).
        //      let piece_len = if self.current_piece_index + 1 == self.total_pieces {
        //          self.total_length - self.current_offset
        //      } else {
        //          self.piece_length
        //      };
        //
        //   2. Read piece data from DiskAdaptor via PieceStorage registry.
        //   3. Compute hash using MessageDigest.
        //   4. Compare with expected hash from DownloadContext registry.
        //   5. Set/unset bit in bitfield accordingly.
        //   6. On I/O error (RecoverableException in C++), unset bit and continue.

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
                dctx_id = self.download_context_id,
                total_pieces = self.total_pieces,
                "PieceHashValidator completed all piece validation"
            );
            // TODO: Sync bitfield back to PieceStorage:
            //   pieceStorage_->setBitfield(bitfield_->getBitfield(), bitfield_->getBitfieldLength());
        }
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

    /// Return the DownloadContext ID reference.
    pub fn download_context_id(&self) -> u64 {
        self.download_context_id
    }

    /// Return the PieceStorage ID reference.
    pub fn piece_storage_id(&self) -> u64 {
        self.piece_storage_id
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
#[derive(Debug)]
pub struct StreamCheckIntegrity {
    /// The validator assigned to this entry.
    /// Equivalent to C++ `CheckIntegrityEntry::validator_`.
    validator: ValidatorKind,
    /// ID reference to the RequestGroup that owns this download.
    request_group_id: u64,
    /// If true, only perform hash checking and do NOT proceed to file allocation
    /// after the check completes. Matches C++ `PREF_HASH_CHECK_ONLY` option.
    hash_check_only: bool,
}

impl StreamCheckIntegrity {
    /// Create a new `StreamCheckIntegrity` entry.
    ///
    /// # Arguments
    ///
    /// * `request_group_id` — ID of the owning RequestGroup.
    /// * `hash_check_only` — If true, skip file allocation after integrity check.
    pub fn new(request_group_id: u64, hash_check_only: bool) -> Self {
        Self {
            validator: ValidatorKind::None,
            request_group_id,
            hash_check_only,
        }
    }

    /// Whether the validation is ready to begin.
    ///
    /// In C++ `PieceHashCheckIntegrityEntry::isValidationReady()`, this checks
    /// `dctx->isPieceHashVerificationAvailable()`. Since we use ID-based
    /// references, the actual check requires registry lookup.
    ///
    /// For now, returns `true` as a placeholder. The real implementation will
    /// look up the DownloadContext from the registry and call
    /// `is_piece_hash_verification_available()`.
    // TODO: Wire up registry lookup for DownloadContext.
    pub fn is_validation_ready(&self) -> bool {
        // C++: dctx->isPieceHashVerificationAvailable()
        // Placeholder — will be resolved via registry.
        true
    }

    /// Initialize the validator for chunk-by-chunk processing.
    ///
    /// In C++ `PieceHashCheckIntegrityEntry::initValidator()`, this creates an
    /// `IteratableChunkChecksumValidator` with the DownloadContext and PieceStorage,
    /// calls `init()` on it, and stores it as the validator.
    ///
    /// The actual creation requires registry lookup. This method creates a
    /// `PieceHashValidator` with placeholder piece count and length values.
    /// TODO: Wire up registry lookup for DownloadContext and PieceStorage.
    pub fn init_validator(&mut self, total_pieces: usize, total_length: u64, piece_length: u64) {
        trace!(
            rg_id = self.request_group_id,
            "StreamCheckIntegrity initializing validator"
        );

        // C++ creates IteratableChunkChecksumValidator(dctx, pieceStorage)
        // then calls validator->init() and setValidator(std::move(validator)).
        let dctx_id = self.request_group_id; // Same RG owns the DownloadContext
        let ps_id = self.request_group_id; // Same RG owns the PieceStorage

        let mut validator = PieceHashValidator::new(
            dctx_id,
            ps_id,
            total_pieces,
            total_length,
            piece_length,
        );
        validator.init();
        self.validator = ValidatorKind::PieceHash(validator);
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
        trace!(
            rg_id = self.request_group_id,
            "StreamCheckIntegrity::on_download_finished (no-op)"
        );
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
            rg_id = self.request_group_id,
            hash_check_only = self.hash_check_only,
            "StreamCheckIntegrity::on_download_incomplete"
        );

        // C++: ps->onDownloadIncomplete()
        // TODO: pieceStorage.on_download_incomplete() via registry.

        if self.hash_check_only {
            trace!(
                rg_id = self.request_group_id,
                "hash_check_only is set — skipping file allocation"
            );
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
        trace!(
            rg_id = self.request_group_id,
            "StreamCheckIntegrity::cut_trailing_garbage"
        );
        // TODO: Resolve PieceStorage from registry, then call disk_adaptor.cut_trailing_garbage().
    }

    /// Whether incomplete validation should be reported as an error.
    ///
    /// Matches C++ `CheckIntegrityEntry::shouldReportIncompleteAsError()`.
    /// Default is `true` for stream downloads.
    pub fn should_report_incomplete_as_error(&self) -> bool {
        true
    }

    /// Return the request group ID reference.
    pub fn request_group_id(&self) -> u64 {
        self.request_group_id
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
#[derive(Debug)]
pub struct BtCheckIntegrity {
    /// The validator assigned to this entry.
    validator: ValidatorKind,
    /// ID reference to the RequestGroup that owns this download.
    request_group_id: u64,
}

impl BtCheckIntegrity {
    /// Create a new `BtCheckIntegrity` entry.
    ///
    /// # Arguments
    ///
    /// * `request_group_id` — ID of the owning RequestGroup.
    pub fn new(request_group_id: u64) -> Self {
        Self {
            validator: ValidatorKind::None,
            request_group_id,
        }
    }

    /// Whether the validation is ready to begin.
    ///
    /// For BT downloads, this checks if piece hash verification is available
    /// in the DownloadContext (BT always has piece hashes from the torrent metadata).
    // TODO: Wire up registry lookup for DownloadContext.
    pub fn is_validation_ready(&self) -> bool {
        // BT downloads always have piece hashes from the .torrent metadata.
        true
    }

    /// Initialize the validator for chunk-by-chunk processing.
    // TODO: Wire up registry lookup for DownloadContext and PieceStorage.
    pub fn init_validator(&mut self, total_pieces: usize, total_length: u64, piece_length: u64) {
        trace!(
            rg_id = self.request_group_id,
            "BtCheckIntegrity initializing validator"
        );

        let dctx_id = self.request_group_id;
        let ps_id = self.request_group_id;

        let mut validator = PieceHashValidator::new(
            dctx_id,
            ps_id,
            total_pieces,
            total_length,
            piece_length,
        );
        validator.init();
        self.validator = ValidatorKind::PieceHash(validator);
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
        trace!(
            rg_id = self.request_group_id,
            "BtCheckIntegrity::on_download_finished (no-op)"
        );
    }

    /// Called when the download is incomplete after integrity check.
    ///
    /// For BT downloads, this signals that some pieces failed verification.
    /// Unlike stream downloads, BT does NOT proceed to file allocation —
    /// the BT pipeline re-downloads missing pieces through its own mechanism.
    // TODO: Wire up PieceStorage::onDownloadIncomplete() via registry.
    pub fn on_download_incomplete(&self) {
        trace!(
            rg_id = self.request_group_id,
            "BtCheckIntegrity::on_download_incomplete"
        );
        // C++: ps->onDownloadIncomplete()
        // No file allocation for BT downloads.
    }

    /// Cut trailing garbage data beyond the expected total length.
    // TODO: Wire up DiskAdaptor::cutTrailingGarbage() via registry.
    pub fn cut_trailing_garbage(&self) {
        trace!(
            rg_id = self.request_group_id,
            "BtCheckIntegrity::cut_trailing_garbage"
        );
    }

    /// Whether incomplete validation should be reported as an error.
    ///
    /// Returns `false` for BT downloads — incomplete pieces are expected
    /// during partial seeding and the BT pipeline handles re-downloading.
    pub fn should_report_incomplete_as_error(&self) -> bool {
        false
    }

    /// Return the request group ID reference.
    pub fn request_group_id(&self) -> u64 {
        self.request_group_id
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
    /// The caller must provide piece metadata since enum dispatch cannot
    /// access the registry directly.
    pub fn init_validator(&mut self, total_pieces: usize, total_length: u64, piece_length: u64) {
        match self {
            CheckIntegrityKind::Stream(s) => s.init_validator(total_pieces, total_length, piece_length),
            CheckIntegrityKind::Bt(b) => b.init_validator(total_pieces, total_length, piece_length),
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

    /// Return the request group ID reference.
    pub fn request_group_id(&self) -> u64 {
        match self {
            CheckIntegrityKind::Stream(s) => s.request_group_id(),
            CheckIntegrityKind::Bt(b) => b.request_group_id(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let v = ValidatorKind::PieceHash(
            PieceHashValidator::new(1, 2, 5, 5_242_880, 1_048_576)
        );
        assert!(!v.is_finished(), "PieceHash with 5 pieces should not be finished initially");
        assert_eq!(v.total_length(), 5_242_880);
        assert_eq!(v.current_offset(), 0);
    }

    // ── PieceHashValidator init and state tracking tests ──────────────────

    #[test]
    fn test_piece_hash_validator_new() {
        let v = PieceHashValidator::new(10, 20, 4, 4_194_304, 1_048_576);
        assert_eq!(v.download_context_id(), 10);
        assert_eq!(v.piece_storage_id(), 20);
        assert_eq!(v.current_piece_index(), 0);
        assert_eq!(v.total_pieces(), 4);
        assert!(!v.is_finished());
        assert_eq!(v.current_offset(), 0);
        assert_eq!(v.total_length(), 4_194_304);
    }

    #[test]
    fn test_piece_hash_validator_zero_pieces_is_finished() {
        let v = PieceHashValidator::new(1, 2, 0, 0, 1_048_576);
        assert!(v.is_finished(), "Zero pieces should be immediately finished");
    }

    #[test]
    fn test_piece_hash_validator_init_resets_state() {
        let mut v = PieceHashValidator::new(1, 2, 3, 3_145_728, 1_048_576);
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
        let mut v = PieceHashValidator::new(1, 2, 0, 0, 1024);
        v.init();
        assert!(v.is_finished(), "Init with zero pieces should set finished");
    }

    // ── Saturated validation progress tests ───────────────────────────────

    #[test]
    fn test_piece_hash_validator_validate_chunk_advances() {
        let mut v = PieceHashValidator::new(1, 2, 3, 3_145_728, 1_048_576);

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
        let mut v = PieceHashValidator::new(1, 2, 2, 2_097_152, 1_048_576);

        v.validate_chunk(); // piece 0 → piece 1
        v.validate_chunk(); // piece 1 → finished

        assert!(v.is_finished());
        // After finishing, offset should not exceed total_length
        assert!(v.current_offset() <= v.total_length());
    }

    // ── Finished flag management tests ────────────────────────────────────

    #[test]
    fn test_piece_hash_validator_finished_after_all_chunks() {
        let mut v = PieceHashValidator::new(1, 2, 2, 2_097_152, 1_048_576);

        assert!(!v.is_finished());
        v.validate_chunk();
        assert!(!v.is_finished());
        v.validate_chunk();
        assert!(v.is_finished());
    }

    #[test]
    fn test_piece_hash_validator_validate_after_finished_is_noop() {
        let mut v = PieceHashValidator::new(1, 2, 1, 1_048_576, 1_048_576);

        v.validate_chunk();
        assert!(v.is_finished());

        // Calling validate_chunk again should not panic or change state
        v.validate_chunk();
        assert!(v.is_finished());
        assert_eq!(v.current_piece_index(), 1);
    }

    // ── CheckIntegrityKind enum dispatch tests ────────────────────────────

    #[test]
    fn test_check_integrity_kind_stream() {
        let entry = CheckIntegrityKind::Stream(
            StreamCheckIntegrity::new(42, false)
        );
        assert_eq!(entry.request_group_id(), 42);
        assert!(entry.is_validation_ready());
        assert!(entry.is_finished()); // No validator yet (None), so finished
        assert_eq!(entry.total_length(), 0);
        assert_eq!(entry.current_length(), 0);
        assert!(entry.should_report_incomplete_as_error());
    }

    #[test]
    fn test_check_integrity_kind_bt() {
        let entry = CheckIntegrityKind::Bt(
            BtCheckIntegrity::new(99)
        );
        assert_eq!(entry.request_group_id(), 99);
        assert!(entry.is_validation_ready());
        assert!(entry.is_finished()); // No validator yet (None), so finished
        assert_eq!(entry.total_length(), 0);
        assert_eq!(entry.current_length(), 0);
        assert!(!entry.should_report_incomplete_as_error());
    }

    #[test]
    fn test_check_integrity_kind_init_and_validate_stream() {
        let mut entry = CheckIntegrityKind::Stream(
            StreamCheckIntegrity::new(1, false)
        );
        entry.init_validator(3, 3_145_728, 1_048_576);
        assert!(!entry.is_finished());

        entry.validate_chunk();
        assert_eq!(entry.current_length(), 1_048_576);

        entry.validate_chunk();
        entry.validate_chunk();
        assert!(entry.is_finished());
    }

    #[test]
    fn test_check_integrity_kind_init_and_validate_bt() {
        let mut entry = CheckIntegrityKind::Bt(
            BtCheckIntegrity::new(2)
        );
        entry.init_validator(2, 2_097_152, 1_048_576);
        assert!(!entry.is_finished());

        entry.validate_chunk();
        entry.validate_chunk();
        assert!(entry.is_finished());
    }

    // ── StreamCheckIntegrity creation and validation_ready tests ──────────

    #[test]
    fn test_stream_check_integrity_new() {
        let s = StreamCheckIntegrity::new(42, false);
        assert_eq!(s.request_group_id(), 42);
        assert!(!s.hash_check_only());
        assert!(s.is_finished()); // No validator → finished
    }

    #[test]
    fn test_stream_check_integrity_hash_check_only() {
        let mut s = StreamCheckIntegrity::new(1, true);
        assert!(s.hash_check_only());
        s.set_hash_check_only(false);
        assert!(!s.hash_check_only());
    }

    #[test]
    fn test_stream_check_integrity_validation_ready() {
        let s = StreamCheckIntegrity::new(1, false);
        // Placeholder returns true
        assert!(s.is_validation_ready());
    }

    #[test]
    fn test_stream_check_integrity_init_validator() {
        let mut s = StreamCheckIntegrity::new(1, false);
        assert!(s.is_finished()); // No validator yet

        s.init_validator(4, 4_194_304, 1_048_576);
        assert!(!s.is_finished()); // Validator created, not yet finished
        assert_eq!(s.total_length(), 4_194_304);
    }

    #[test]
    fn test_stream_check_integrity_validator_access() {
        let mut s = StreamCheckIntegrity::new(1, false);
        assert!(matches!(s.validator(), ValidatorKind::None));

        s.init_validator(1, 1_048_576, 1_048_576);
        assert!(matches!(s.validator(), ValidatorKind::PieceHash(_)));
    }

    #[test]
    fn test_stream_check_integrity_on_download_finished_noop() {
        let s = StreamCheckIntegrity::new(1, false);
        // Should not panic
        s.on_download_finished();
    }

    #[test]
    fn test_stream_check_integrity_on_download_incomplete() {
        let s = StreamCheckIntegrity::new(1, false);
        // Should not panic
        s.on_download_incomplete();
    }

    #[test]
    fn test_stream_check_integrity_hash_check_only_skips_allocation() {
        // This test verifies the hash_check_only path logic.
        // The actual file allocation dispatch is TODO, but we verify
        // the method runs without panic for both branches.
        let s_with = StreamCheckIntegrity::new(1, true);
        let s_without = StreamCheckIntegrity::new(1, false);
        s_with.on_download_incomplete();
        s_without.on_download_incomplete();
    }

    // ── BtCheckIntegrity tests ────────────────────────────────────────────

    #[test]
    fn test_bt_check_integrity_new() {
        let b = BtCheckIntegrity::new(7);
        assert_eq!(b.request_group_id(), 7);
        assert!(b.is_finished()); // No validator yet
        assert!(!b.should_report_incomplete_as_error());
    }

    #[test]
    fn test_bt_check_integrity_init_validator() {
        let mut b = BtCheckIntegrity::new(1);
        b.init_validator(2, 2_097_152, 1_048_576);
        assert!(!b.is_finished());
        assert_eq!(b.total_length(), 2_097_152);
    }

    #[test]
    fn test_bt_check_integrity_on_download_handlers() {
        let b = BtCheckIntegrity::new(1);
        // Should not panic
        b.on_download_finished();
        b.on_download_incomplete();
    }

    // ── Cross-cutting: ValidatorKind after PieceHashValidator assignment ───

    #[test]
    fn test_validator_kind_piece_hash_full_lifecycle() {
        let mut v = ValidatorKind::PieceHash(
            PieceHashValidator::new(1, 2, 2, 2_097_152, 1_048_576)
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
        let mut v = ValidatorKind::PieceHash(
            PieceHashValidator::new(1, 2, 3, 3_145_728, 1_048_576)
        );
        v.validate_chunk(); // advance to piece 1
        assert_eq!(v.current_offset(), 1_048_576);

        v.init(); // reset
        assert_eq!(v.current_offset(), 0);
        assert!(!v.is_finished());
    }
}
