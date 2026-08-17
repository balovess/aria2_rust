//! Rust-owned descriptions of the command work that follows integrity checks.
//!
//! The original client performs these callbacks by mutating a request group
//! and pushing commands into its event loop. Rust's production integrity path
//! instead uses [`super::man::CheckIntegrityTask`] and async managers. These
//! small value types keep the legacy entry wrappers useful without hiding a
//! registry lookup or blocking an async executor.

use std::path::PathBuf;

/// One physical file participating in an integrity callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityFile {
    /// Local path owned by the download context.
    pub path: PathBuf,
    /// Declared length of the file.
    pub length: u64,
}

impl IntegrityFile {
    pub fn new(path: PathBuf, length: u64) -> Self {
        Self { path, length }
    }
}

/// Work required after a piece-hash check finds incomplete data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityIncompleteAction {
    /// The owning command must invoke `PieceStorage::on_download_incomplete`.
    pub reset_piece_storage: bool,
    /// Files to pass to the existing file-allocation manager, or `None` when
    /// hash-check-only mode stops before allocation.
    pub file_allocation: Option<Vec<IntegrityFile>>,
}

impl IntegrityIncompleteAction {
    pub fn new(
        reset_piece_storage: bool,
        hash_check_only: bool,
        files: Vec<IntegrityFile>,
    ) -> Self {
        Self {
            reset_piece_storage,
            file_allocation: (!hash_check_only).then_some(files),
        }
    }

    /// Apply the lifecycle part that requires mutable piece-storage ownership.
    pub fn apply_piece_storage(
        &self,
        piece_storage: &mut dyn crate::segment::piece_storage::PieceStorage,
    ) {
        if self.reset_piece_storage {
            piece_storage.on_download_incomplete();
        }
    }
}

/// Work required to remove bytes beyond the declared download length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityTrailingGarbageAction {
    /// Physical files and their declared lengths.
    pub files: Vec<IntegrityFile>,
}

impl IntegrityTrailingGarbageAction {
    pub fn new(files: Vec<IntegrityFile>) -> Self {
        Self { files }
    }

    pub fn single_file(path: PathBuf, length: u64) -> Self {
        Self::new(vec![IntegrityFile::new(path, length)])
    }

    /// Apply truncation through the existing async integrity helper.
    pub async fn apply(&self) -> crate::error::Result<()> {
        let files: Vec<_> = self
            .files
            .iter()
            .map(|file| (file.path.clone(), file.length))
            .collect();
        super::man::cut_multi_file_trailing_garbage(&files).await
    }
}

/// Work required after a successful integrity check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntegrityFinishedAction {
    /// Files to pass to the existing file-allocation manager, if applicable.
    pub file_allocation: Option<Vec<IntegrityFile>>,
    /// Whether the owning command should emit its BT completion hook.
    pub run_completion_hook: bool,
}

impl IntegrityFinishedAction {
    pub fn for_bt(
        files: Vec<IntegrityFile>,
        hash_check_only: bool,
        hash_check_seed: bool,
        run_completion_hook: bool,
    ) -> Self {
        Self {
            file_allocation: (!hash_check_only && hash_check_seed).then_some(files),
            run_completion_hook,
        }
    }
}
