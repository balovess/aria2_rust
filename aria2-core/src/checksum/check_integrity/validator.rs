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
                tracing::trace!("ValidatorKind::init called on None variant — no-op");
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
                tracing::trace!("ValidatorKind::validate_chunk called on None variant — no-op");
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
    pub fn apply_validation_results(
        &self,
        ps: &mut dyn crate::segment::piece_storage::PieceStorage,
    ) {
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
    download_context: std::sync::Arc<crate::download::DownloadContext>,
    /// Shared piece storage holding the download bitfield and disk adaptor.
    /// C++ uses `shared_ptr<PieceStorage>`.
    /// Used for `read_data()` (`&self` method). Mutation is done via
    /// `apply_validation_results()` which takes `&mut dyn PieceStorage`.
    piece_storage: std::sync::Arc<dyn crate::segment::piece_storage::PieceStorage>,
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
        download_context: std::sync::Arc<crate::download::DownloadContext>,
        piece_storage: std::sync::Arc<dyn crate::segment::piece_storage::PieceStorage>,
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
        tracing::trace!(
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
            tracing::trace!("PieceHashValidator::validate_chunk called after completion — no-op");
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
                        tracing::trace!(piece_index, "Piece hash verified OK");
                        self.validation_results
                            .push(PieceValidationResult::Verified { piece_index });
                        self.pieces_ok += 1;
                    } else {
                        // Hash mismatch — record failed result.
                        tracing::warn!(
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
                    tracing::trace!(
                        piece_index,
                        "No expected hash for piece — skipping verification"
                    );
                }
            }
            Ok(data) => {
                // Read returned wrong length — treat as failure.
                tracing::warn!(
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
                tracing::warn!(
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
            tracing::info!(
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
    pub fn download_context(&self) -> &std::sync::Arc<crate::download::DownloadContext> {
        &self.download_context
    }

    /// Return a reference to the PieceStorage.
    pub fn piece_storage(
        &self,
    ) -> &std::sync::Arc<dyn crate::segment::piece_storage::PieceStorage> {
        &self.piece_storage
    }
}
