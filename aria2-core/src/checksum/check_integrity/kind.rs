// ---------------------------------------------------------------------------
// CheckIntegrityKind — replaces C++ CheckIntegrityEntry hierarchy
// ---------------------------------------------------------------------------

use super::{BtCheckIntegrity, StreamCheckIntegrity};

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
