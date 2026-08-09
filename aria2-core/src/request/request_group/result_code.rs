//! Structured download result codes.
//!
//! Port of C++ `error_code.h` / `error_code.cc`. Provides a typed enum for
//! the original download result codes instead of free-form error strings,
//! enabling RPC consumers (web UIs, scripts) to distinguish timeout, network
//! failure, user removal, and other outcomes.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Structured result code for a completed download.
///
/// Mirrors every wire-visible value in C++ `error_code::Value`.
///
/// Paused is deliberately not a variant: aria2 represents it as a task
/// status, not as an error code. Keeping that distinction prevents a Rust-
/// only value from leaking into `errorCode` in stopped-download responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
#[derive(Default)]
pub enum DownloadResultCode {
    /// Download completed successfully.
    Finished = 0,
    /// Unknown error occurred.
    #[default]
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
    /// Duplicate download (same URI already queued).
    DuplicateDownload = 11,
    /// Duplicate BitTorrent info hash.
    DuplicateInfoHash = 12,
    /// Output file already exists.
    FileAlreadyExists = 13,
    /// Output file rename failed.
    FileRenamingFailed = 14,
    /// Output file open failed.
    FileOpenError = 15,
    /// Output file creation failed.
    FileCreateError = 16,
    /// Output file I/O failed.
    FileIoError = 17,
    /// Directory creation failed.
    DirCreateError = 18,
    /// Name resolution failed.
    NameResolveError = 19,
    /// Metalink parsing failed.
    MetalinkParseError = 20,
    /// FTP protocol error.
    FtpProtocolError = 21,
    /// HTTP protocol error.
    HttpProtocolError = 22,
    /// Too many HTTP redirects.
    HttpTooManyRedirects = 23,
    /// HTTP authentication failed.
    HttpAuthFailed = 24,
    /// Bencode parsing failed.
    BencodeParseError = 25,
    /// BitTorrent parsing failed.
    BittorrentParseError = 26,
    /// Magnet parsing failed.
    MagnetParseError = 27,
    /// Option validation failed.
    OptionError = 28,
    /// HTTP service unavailable.
    HttpServiceUnavailable = 29,
    /// JSON parsing failed.
    JsonParseError = 30,
    /// User-requested removal (aria2.remove / aria2.forceRemove).
    Removed = 31,
    /// Checksum verification failed.
    ChecksumError = 32,
}

impl DownloadResultCode {
    /// Return `true` if the result indicates a successful completion.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Finished)
    }

    /// Return `true` if the download was interrupted but not failed
    /// (i.e. can be resumed on next startup).
    pub fn is_resumable(self) -> bool {
        matches!(self, Self::InProgress)
    }

    /// Return `true` if the download was explicitly stopped by the user.
    pub fn is_user_stopped(self) -> bool {
        matches!(self, Self::Removed)
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
            12 => Some(Self::DuplicateInfoHash),
            13 => Some(Self::FileAlreadyExists),
            14 => Some(Self::FileRenamingFailed),
            15 => Some(Self::FileOpenError),
            16 => Some(Self::FileCreateError),
            17 => Some(Self::FileIoError),
            18 => Some(Self::DirCreateError),
            19 => Some(Self::NameResolveError),
            20 => Some(Self::MetalinkParseError),
            21 => Some(Self::FtpProtocolError),
            22 => Some(Self::HttpProtocolError),
            23 => Some(Self::HttpTooManyRedirects),
            24 => Some(Self::HttpAuthFailed),
            25 => Some(Self::BencodeParseError),
            26 => Some(Self::BittorrentParseError),
            27 => Some(Self::MagnetParseError),
            28 => Some(Self::OptionError),
            29 => Some(Self::HttpServiceUnavailable),
            30 => Some(Self::JsonParseError),
            31 => Some(Self::Removed),
            32 => Some(Self::ChecksumError),
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
            Self::DuplicateInfoHash => "duplicate_info_hash",
            Self::FileAlreadyExists => "file_already_exists",
            Self::FileRenamingFailed => "file_renaming_failed",
            Self::FileOpenError => "file_open_error",
            Self::FileCreateError => "file_create_error",
            Self::FileIoError => "file_io_error",
            Self::DirCreateError => "dir_create_error",
            Self::NameResolveError => "name_resolve_error",
            Self::MetalinkParseError => "metalink_parse_error",
            Self::FtpProtocolError => "ftp_protocol_error",
            Self::HttpProtocolError => "http_protocol_error",
            Self::HttpTooManyRedirects => "http_too_many_redirects",
            Self::HttpAuthFailed => "http_auth_failed",
            Self::BencodeParseError => "bencode_parse_error",
            Self::BittorrentParseError => "bittorrent_parse_error",
            Self::MagnetParseError => "magnet_parse_error",
            Self::OptionError => "option_error",
            Self::HttpServiceUnavailable => "http_service_unavailable",
            Self::JsonParseError => "json_parse_error",
            Self::ChecksumError => "checksum_error",
            Self::Removed => "removed",
        };
        write!(f, "{}", s)
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
    fn test_cpp_wire_code_assignments() {
        assert_eq!(DownloadResultCode::DuplicateInfoHash.as_code(), 12);
        assert_eq!(DownloadResultCode::HttpAuthFailed.as_code(), 24);
        assert_eq!(DownloadResultCode::JsonParseError.as_code(), 30);
        assert_eq!(DownloadResultCode::Removed.as_code(), 31);
        assert_eq!(DownloadResultCode::ChecksumError.as_code(), 32);
        assert_eq!(DownloadResultCode::from_code(33), None);
    }

    #[test]
    fn test_is_success() {
        assert!(DownloadResultCode::Finished.is_success());
        assert!(!DownloadResultCode::TimeOut.is_success());
    }

    #[test]
    fn test_is_resumable() {
        assert!(DownloadResultCode::InProgress.is_resumable());
        assert!(!DownloadResultCode::TimeOut.is_resumable());
    }

    #[test]
    fn test_is_user_stopped() {
        assert!(DownloadResultCode::Removed.is_user_stopped());
        assert!(!DownloadResultCode::InProgress.is_user_stopped());
    }
}
