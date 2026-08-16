// BtCheckIntegrity replaces the implied C++ BtCheckIntegrityEntry.

use std::sync::Arc;

use super::{
    IntegrityFile, IntegrityFinishedAction, IntegrityIncompleteAction,
    IntegrityTrailingGarbageAction, PieceHashValidator, ValidatorKind,
};

/// Integrity checking entry for BitTorrent downloads.
///
/// Lifecycle methods return explicit dispatch plans. The owning BT command
/// applies those plans through its mutable piece storage and async managers.
pub struct BtCheckIntegrity {
    validator: ValidatorKind,
    download_context: Option<Arc<crate::download::DownloadContext>>,
    piece_storage: Option<Arc<dyn crate::segment::piece_storage::PieceStorage>>,
    hash_check_only: bool,
    hash_check_seed: bool,
    enable_completion_hook: bool,
}

impl std::fmt::Debug for BtCheckIntegrity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtCheckIntegrity")
            .field("validator", &self.validator)
            .field("download_context", &self.download_context.is_some())
            .field("piece_storage", &self.piece_storage.is_some())
            .field("hash_check_only", &self.hash_check_only)
            .field("hash_check_seed", &self.hash_check_seed)
            .field("enable_completion_hook", &self.enable_completion_hook)
            .finish()
    }
}

impl BtCheckIntegrity {
    /// Create a BT entry with the default post-check behavior.
    pub fn new(
        download_context: Arc<crate::download::DownloadContext>,
        piece_storage: Arc<dyn crate::segment::piece_storage::PieceStorage>,
    ) -> Self {
        Self {
            validator: ValidatorKind::None,
            download_context: Some(download_context),
            piece_storage: Some(piece_storage),
            hash_check_only: false,
            hash_check_seed: true,
            enable_completion_hook: false,
        }
    }

    /// Create a BT entry with explicit post-check options.
    pub fn new_with_options(
        download_context: Arc<crate::download::DownloadContext>,
        piece_storage: Arc<dyn crate::segment::piece_storage::PieceStorage>,
        hash_check_only: bool,
        hash_check_seed: bool,
        enable_completion_hook: bool,
    ) -> Self {
        Self {
            hash_check_only,
            hash_check_seed,
            enable_completion_hook,
            ..Self::new(download_context, piece_storage)
        }
    }

    /// Whether the validation is ready to begin.
    pub fn is_validation_ready(&self) -> bool {
        self.download_context
            .as_ref()
            .is_none_or(|ctx| !ctx.get_piece_hashes().is_empty())
    }

    /// Initialize the validator for chunk-by-chunk processing.
    pub fn init_validator(&mut self) {
        tracing::trace!("BtCheckIntegrity initializing validator");

        if let (Some(ctx), Some(ps)) = (&self.download_context, &self.piece_storage) {
            let total_pieces = ctx.get_piece_hashes().len();
            if total_pieces == 0 {
                tracing::trace!("No piece hashes available; skipping validator creation");
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

    /// Validate a single chunk.
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

    /// Current validated byte length.
    pub fn current_length(&self) -> u64 {
        self.validator.current_offset()
    }

    /// Build the post-success dispatch plan.
    pub fn on_download_finished(&self) -> IntegrityFinishedAction {
        IntegrityFinishedAction {
            file_allocation: (!self.hash_check_only && self.hash_check_seed).then(|| self.files()),
            run_completion_hook: self.enable_completion_hook,
        }
    }

    /// Build the dispatch plan for an incomplete BT download.
    pub fn on_download_incomplete(&self) -> IntegrityIncompleteAction {
        IntegrityIncompleteAction {
            reset_piece_storage: self.piece_storage.is_some(),
            file_allocation: (!self.hash_check_only).then(|| self.files()),
        }
    }

    /// Build the request for removing bytes beyond the declared file lengths.
    pub fn cut_trailing_garbage(&self) -> IntegrityTrailingGarbageAction {
        IntegrityTrailingGarbageAction {
            files: self.files(),
        }
    }

    /// Whether incomplete validation should be reported as an error.
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

    fn files(&self) -> Vec<IntegrityFile> {
        self.download_context
            .as_ref()
            .map(|ctx| {
                ctx.get_file_entries()
                    .iter()
                    .map(|entry| IntegrityFile {
                        path: entry.path().into(),
                        length: entry.length(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
