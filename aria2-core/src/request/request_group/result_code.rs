//! Structured download result codes.
//!
//! Port of C++ `error_code.h` / `error_code.cc`. Provides a typed enum for
//! download outcomes instead of free-form error strings, enabling RPC
//! consumers (web UIs, scripts) to programmatically distinguish between
//! timeout, network failure, user removal, etc.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Structured result code for a completed download.
///
/// Mirrors C++ `error_code::Value` with the most commonly used variants.
/// The numeric values match the C++ wire format for RPC compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
pub enum DownloadResultCode {
    /// Download completed successfully.
    Finished = 0,
    /// Unknown error occurred.
    UnknownError = 1,
    /// Connection / read timed out.
    TimeOut = 2,
    /// Resource not found (HTTP 404, etc.).
    ResourceNotFound = 3,
    /// Max file-not-found count exceeded.
    MaxFileNotFound = 4,
    /// Download speed too slow.
    TooSlow = 5,
    /// Network problem (connection reset, DNS failure, etc.).
    NetworkProblem = 6,
    /// Download interrupted (e.g. by shutdown), not failed.
    InProgress = 7,
    /// Cannot resume the download (server doesn't support range requests).
    CannotResume = 8,
    /// Not enough disk space.
    NotEnoughDiskSpace = 9,
    /// Piece length changed from the previous session.
    PieceLengthChanged = 10,
    /// Duplicate download (same info-hash or URI already queued).
    DuplicateDownload = 11,
    /// Checksum verification failed.
    ChecksumError = 12,
    /// User-requested removal (aria2.remove / aria2.forceRemove).
    Removed = 31,
    /// Download was paused and never completed.
    Paused = 32,
}

impl DownloadResultCode {
    /// Return `true` if the result indicates a successful completion.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Finished)
    }

    /// Return `true` if the download was interrupted but not failed
    /// (i.e. can be resumed on next startup).
    pub fn is_resumable(self) -> bool {
        matches!(self, Self::InProgress | Self::Paused)
    }

    /// Return `true` if the download was explicitly stopped by the user.
    pub fn is_user_stopped(self) -> bool {
        matches!(self, Self::Removed | Self::Paused)
    }

    /// Convert from a numeric code (matching C++ wire format).
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Finished),
            1 => Some(Self::UnknownError),
            2 => Some(Self::TimeOut),
            3 => Some(Self::ResourceNotFound),
            4 => Some(Self::MaxFileNotFound),
            5 => Some(Self::TooSlow),
            6 => Some(Self::NetworkProblem),
            7 => Some(Self::InProgress),
            8 => Some(Self::CannotResume),
            9 => Some(Self::NotEnoughDiskSpace),
            10 => Some(Self::PieceLengthChanged),
            11 => Some(Self::DuplicateDownload),
            12 => Some(Self::ChecksumError),
            31 => Some(Self::Removed),
            32 => Some(Self::Paused),
            _ => None,
        }
    }

    /// Convert to numeric code (matching C++ wire format).
    pub fn as_code(self) -> u32 {
        self as u32
    }
}

impl fmt::Display for DownloadResultCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Finished => "finished",
            Self::UnknownError => "unknown_error",
            Self::TimeOut => "timeout",
            Self::ResourceNotFound => "resource_not_found",
            Self::MaxFileNotFound => "max_file_not_found",
            Self::TooSlow => "too_slow",
            Self::NetworkProblem => "network_problem",
            Self::InProgress => "in_progress",
            Self::CannotResume => "cannot_resume",
            Self::NotEnoughDiskSpace => "not_enough_disk_space",
            Self::PieceLengthChanged => "piece_length_changed",
            Self::DuplicateDownload => "duplicate_download",
            Self::ChecksumError => "checksum_error",
            Self::Removed => "removed",
            Self::Paused => "paused",
        };
        write!(f, "{}", s)
    }
}

impl Default for DownloadResultCode {
    fn default() -> Self {
        Self::UnknownError
    }
}

/// Result of a completed download attempt.
///
/// Mirrors C++ `DownloadResult` which carries both the structured code
/// and a human-readable message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadResult {
    /// Structured result code.
    pub code: DownloadResultCode,
    /// Human-readable error / status message.
    pub message: String,
}

impl DownloadResult {
    /// Create a successful result.
    pub fn finished() -> Self {
        Self {
            code: DownloadResultCode::Finished,
            message: String::from("OK"),
        }
    }

    /// Create a result for a user-removed download.
    pub fn removed() -> Self {
        Self {
            code: DownloadResultCode::Removed,
            message: String::from("Download removed by user"),
        }
    }

    /// Create a result for an interrupted (shutdown) download.
    pub fn in_progress() -> Self {
        Self {
            code: DownloadResultCode::InProgress,
            message: String::from("Download interrupted by shutdown"),
        }
    }

    /// Create a result for a paused download.
    pub fn paused() -> Self {
        Self {
            code: DownloadResultCode::Paused,
            message: String::from("Download paused"),
        }
    }

    /// Create an error result with a specific code and message.
    pub fn error(code: DownloadResultCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_result_code_roundtrip() {
        for code in [
            DownloadResultCode::Finished,
            DownloadResultCode::TimeOut,
            DownloadResultCode::Removed,
            DownloadResultCode::InProgress,
            DownloadResultCode::ChecksumError,
        ] {
            assert_eq!(DownloadResultCode::from_code(code.as_code()), Some(code));
        }
    }

    #[test]
    fn test_is_success() {
        assert!(DownloadResultCode::Finished.is_success());
        assert!(!DownloadResultCode::TimeOut.is_success());
    }

    #[test]
    fn test_is_resumable() {
        assert!(DownloadResultCode::InProgress.is_resumable());
        assert!(DownloadResultCode::Paused.is_resumable());
        assert!(!DownloadResultCode::TimeOut.is_resumable());
    }

    #[test]
    fn test_is_user_stopped() {
        assert!(DownloadResultCode::Removed.is_user_stopped());
        assert!(DownloadResultCode::Paused.is_user_stopped());
        assert!(!DownloadResultCode::InProgress.is_user_stopped());
    }

    #[test]
    fn test_download_result_finished() {
        let r = DownloadResult::finished();
        assert_eq!(r.code, DownloadResultCode::Finished);
    }

    #[test]
    fn test_download_result_removed() {
        let r = DownloadResult::removed();
        assert_eq!(r.code, DownloadResultCode::Removed);
    }

    #[test]
    fn test_download_result_in_progress() {
        let r = DownloadResult::in_progress();
        assert_eq!(r.code, DownloadResultCode::InProgress);
    }
}
