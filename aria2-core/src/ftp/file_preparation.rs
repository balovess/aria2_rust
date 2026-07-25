//! FTP file preparation pipeline — post-SIZE logic.
//!
//! Equivalent to C++ `FtpNegotiationCommand::onFileSizeDetermined()`.
//!
//! After the FTP SIZE command returns (or doesn't), the download pipeline
//! must decide how to proceed based on the file size and various options.
//! This module encapsulates that decision logic as a pure data transformation,
//! making it independently unit-testable without requiring network I/O or
//! the full RequestGroup infrastructure.
//!
//! # C++ Flow Mapping
//!
//! | C++ Step                              | Rust Implementation                |
//! |---------------------------------------|------------------------------------|
//! | Overflow check (size > a2_off_t max) | `prepare_ftp_file()` → `Err`      |
//! | Zero-length detection                 | `FtpFileSize::ZeroLength`          |
//! | Dry-run mode                          | `FtpFilePreparationConfig.dry_run` |
//! | File path derivation                  | `derive_local_path()`              |
//! | markTotalLengthIsUnknown()            | `FtpFileSize::Unknown`             |
//! | PieceStorage init (deferred)          | TODO markers in result variants    |
//! | validateTotalLength (deferred)        | TODO markers in result variants    |

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError};

// =============================================================================
// Core types
// =============================================================================

/// Outcome of the FTP SIZE command, captured before file preparation begins.
///
/// This enum distinguishes between three states that the C++ code handles
/// differently in `onFileSizeDetermined()` vs `recvSize()`:
///
/// - **Known size** (SIZE returned 213): normal or zero-length path
/// - **Unknown size** (SIZE returned non-213): disables segmented download
///   and resume, matching C++ `markTotalLengthIsUnknown()`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtpFileSize {
    /// Server returned a valid 213 response with the file size.
    Known(u64),
    /// Server doesn't support SIZE (non-213 response).
    /// C++ calls `markTotalLengthIsUnknown()` and `onFileSizeDetermined(0)`.
    Unknown,
}

/// Result of the FTP file preparation pipeline.
///
/// Each variant corresponds to a distinct control-flow path in the C++
/// `onFileSizeDetermined()`, allowing the caller to take the appropriate
/// next step without embedding the full decision tree in the negotiation code.
#[derive(Debug, Clone, PartialEq)]
pub enum FtpFilePreparationResult {
    /// Normal download: file has known non-zero size, ready to proceed.
    NormalDownload {
        /// The file size in bytes.
        total_length: u64,
        /// The derived local file path (dir + safe filename).
        local_path: PathBuf,
        /// The suffix path (filename only, before dir prefix).
        suffix_path: String,
    },

    /// Zero-length file detected. C++ has complex branching here:
    /// - dry-run → mark pieces done, pool connection
    /// - file already exists with matching length → checksum or mark done
    /// - otherwise → create zero-length file on disk
    ZeroLengthFile {
        /// The derived local file path.
        local_path: PathBuf,
        /// The suffix path (filename only).
        suffix_path: String,
        /// Whether dry-run mode is active.
        dry_run: bool,
        /// Whether a file with matching length already exists on disk.
        /// C++ `downloadFinishedByFileLength()` checks this.
        file_exists_with_matching_length: bool,
        /// Whether checksum verification is needed (from DownloadContext).
        checksum_verification_needed: bool,
    },

    /// Total length is unknown (SIZE not supported by server).
    /// C++ calls `markTotalLengthIsUnknown()` which disables segmented
    /// downloading and resume. The download proceeds in single-connection
    /// "grow" mode using `UnknownLengthPieceStorage`.
    TotalLengthUnknown {
        /// The derived local file path.
        local_path: PathBuf,
        /// The suffix path (filename only).
        suffix_path: String,
    },

    /// Dry-run mode: file found, mark pieces done and pool the connection.
    /// C++ `onDryRunFileFound()` path. No actual download occurs.
    DryRunFileFound {
        /// The file size in bytes (0 if unknown).
        total_length: u64,
        /// The derived local file path.
        local_path: PathBuf,
        /// The suffix path (filename only).
        suffix_path: String,
    },

    /// Download already completed (zero-length file exists, no checksum needed).
    /// C++ sets `sequence_ = SEQ_DOWNLOAD_ALREADY_COMPLETED`.
    DownloadAlreadyCompleted {
        /// The local file path.
        local_path: PathBuf,
    },

    /// Checksum verification needed for zero-length file.
    /// C++ pushes a `ChecksumCheckIntegrityEntry` and sets `sequence_ = SEQ_EXIT`.
    ChecksumVerificationNeeded {
        /// The local file path.
        local_path: PathBuf,
    },
}

/// Configuration for the FTP file preparation pipeline.
///
/// Contains the options and state that C++ reads from `getOption()`,
/// `getFileEntry()`, and `getDownloadContext()` during `onFileSizeDetermined()`.
#[derive(Debug, Clone)]
pub struct FtpFilePreparationConfig {
    /// Output directory (C++ `PREF_DIR`). Defaults to ".".
    pub dir: String,

    /// Whether dry-run mode is enabled (C++ `PREF_DRY_RUN`).
    pub dry_run: bool,

    /// The request URL, used to derive the local filename.
    /// C++ reads `getRequest()->getFile()` which is the URL-decoded path.
    pub request_file: String,

    /// Whether the local file path has already been set on the FileEntry.
    /// C++ checks `getFileEntry()->getPath().empty()`.
    pub path_already_set: bool,

    /// Whether the existing file on disk matches the expected length.
    /// C++ `downloadFinishedByFileLength()`.
    pub file_exists_with_matching_length: bool,

    /// Whether checksum verification is needed.
    /// C++ `getDownloadContext()->isChecksumVerificationNeeded()`.
    pub checksum_verification_needed: bool,

    /// Whether the download context knows the total length.
    /// C++ `getDownloadContext()->knowsTotalLength()`.
    pub knows_total_length: bool,

    /// Whether PieceStorage has already been initialized.
    /// C++ checks `getPieceStorage()` being non-null.
    /// When true, the preparation should validate rather than initialize.
    pub piece_storage_initialized: bool,
}

impl Default for FtpFilePreparationConfig {
    fn default() -> Self {
        Self {
            dir: ".".to_string(),
            dry_run: false,
            request_file: String::new(),
            path_already_set: false,
            file_exists_with_matching_length: false,
            checksum_verification_needed: false,
            knows_total_length: true,
            piece_storage_initialized: false,
        }
    }
}

// =============================================================================
// Path derivation (C++ util::createSafePath + percentDecode + applyDir)
// =============================================================================

/// Derive the local file path from the request URL path.
///
/// Matches the C++ flow:
/// 1. `util::percentDecode(getRequest()->getFile())` — decode URL-encoded chars
/// 2. `util::createSafePath(decoded)` — replace `/` and `\` with `_`, strip `\0`
/// 3. `util::applyDir(PREF_DIR, suffixPath)` — prepend output directory
///
/// Returns `(local_path, suffix_path)` where `suffix_path` is the safe filename
/// and `local_path` is `dir/suffix_path`.
pub fn derive_local_path(dir: &str, request_file: &str) -> (PathBuf, String) {
    let decoded = percent_decode(request_file);
    let suffix_path = create_safe_path(&decoded);
    let local_path = apply_dir(dir, &suffix_path);
    (local_path, suffix_path)
}

/// Percent-decode a string, handling multi-byte UTF-8 sequences correctly.
///
/// Matches C++ `util::percentDecode()` applied to file paths.
/// Unlike the HTTP `percent_decode_str` (which only handles ASCII bytes),
/// this version correctly handles multi-byte UTF-8 sequences like `%E6%96%87`.
pub fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
            } else {
                // Invalid percent-encoding, preserve literal characters
                bytes.push(b'%');
                bytes.extend_from_slice(hex.as_bytes());
            }
        } else {
            // Handle multi-byte UTF-8 characters by pushing all their bytes
            let mut buf = [0u8; 4];
            let str_slice = c.encode_utf8(&mut buf);
            bytes.extend_from_slice(str_slice.as_bytes());
        }
    }
    // Decode the full byte sequence as UTF-8, with lossy fallback
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Create a safe filename by replacing directory separators and null bytes.
///
/// Matches C++ `util::createSafePath()`:
/// - Replace `/` and `\` with `_`
/// - Strip null bytes (`\0`)
/// - Return "index.html" if the result is empty
pub fn create_safe_path(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' => result.push('_'),
            '\0' => { /* strip null bytes */ }
            _ => result.push(ch),
        }
    }
    if result.is_empty() {
        "index.html".to_string()
    } else {
        result
    }
}

/// Prepend the output directory to a filename.
///
/// Matches C++ `util::applyDir(PREF_DIR, suffixPath)`:
/// - If `dir` is empty or ".", return just the filename
/// - Otherwise return `dir/filename`
pub fn apply_dir(dir: &str, suffix_path: &str) -> PathBuf {
    if dir.is_empty() || dir == "." {
        PathBuf::from(suffix_path)
    } else {
        Path::new(dir).join(suffix_path)
    }
}

// =============================================================================
// Main entry point
// =============================================================================

/// Execute the FTP file preparation pipeline.
///
/// This is the Rust equivalent of C++ `FtpNegotiationCommand::onFileSizeDetermined()`
/// and the post-SIZE portion of `recvSize()`.
///
/// # Errors
///
/// Returns `Aria1Error::Recoverable(FtpProtocolError)` if the file size
/// overflows the internal representation (C++ `EX_TOO_LARGE_FILE`).
///
/// # Control Flow (matching C++)
///
/// ```text
/// SIZE response
///   ├─ 213 (known size)
///   │   ├─ overflow? → Error
///   │   ├─ piece_storage already exists? → validate total length
///   │   ├─ dry_run? → DryRunFileFound
///   │   ├─ size == 0?
///   │   │   ├─ dry_run? → DryRunFileFound
///   │   │   ├─ file exists + matching length?
///   │   │   │   ├─ checksum needed? → ChecksumVerificationNeeded
///   │   │   │   └─ else → DownloadAlreadyCompleted
///   │   │   └─ else → ZeroLengthFile (create on disk)
///   │   └─ size > 0 → NormalDownload
///   └─ non-213 (unknown size)
///       └─ TotalLengthUnknown
/// ```
pub fn prepare_ftp_file(
    file_size: &FtpFileSize,
    config: &FtpFilePreparationConfig,
) -> Result<FtpFilePreparationResult, Aria2Error> {
    match file_size {
        FtpFileSize::Known(size) => {
            // C++ recvSize(): overflow check
            if *size > i64::MAX as u64 {
                warn!("File size {} exceeds maximum representable size", size);
                return Err(Aria2Error::Recoverable(
                    RecoverableError::FtpProtocolError {
                        message: format!("File too large: {} bytes exceeds i64::MAX", size),
                    },
                ));
            }

            // C++ recvSize(): if PieceStorage already exists, validate total length
            if config.piece_storage_initialized {
                // TODO: Call RequestGroup::validateTotalLength() once
                // RequestGroup infrastructure is built. For now, just log.
                debug!(
                    "PieceStorage already initialized, validating total length: {}",
                    size
                );
            }

            // Derive file path if not already set (C++ checks getFileEntry()->getPath().empty())
            let (local_path, suffix_path) = if config.path_already_set {
                // Path already determined by a prior step (e.g., HTTP redirect)
                (PathBuf::from(&config.request_file), config.request_file.clone())
            } else {
                derive_local_path(&config.dir, &config.request_file)
            };

            let size = *size;

            if size == 0 {
                handle_zero_length_file(config, local_path, suffix_path)
            } else if config.dry_run {
                // C++ onFileSizeDetermined() non-zero branch: dry-run
                info!(
                    "Dry-run: file found, size={} path={}",
                    size,
                    local_path.display()
                );
                Ok(FtpFilePreparationResult::DryRunFileFound {
                    total_length: size,
                    local_path,
                    suffix_path,
                })
            } else {
                // Normal download path
                info!(
                    "File preparation complete: size={} path={}",
                    size,
                    local_path.display()
                );
                Ok(FtpFilePreparationResult::NormalDownload {
                    total_length: size,
                    local_path,
                    suffix_path,
                })
            }
        }

        FtpFileSize::Unknown => {
            // C++ recvSize() non-213 branch:
            // "The remote FTP Server doesn't recognize SIZE command."
            // markTotalLengthIsUnknown() disables segmented downloading and resume.
            debug!("SIZE not supported by server, total length unknown");

            let (local_path, suffix_path) = if config.path_already_set {
                (PathBuf::from(&config.request_file), config.request_file.clone())
            } else {
                derive_local_path(&config.dir, &config.request_file)
            };

            // In the unknown-length path, C++ also checks dry-run
            // (C++ calls onFileSizeDetermined(0) which hits the zero-length branch)
            if config.dry_run {
                info!(
                    "Dry-run: file found (unknown size), path={}",
                    local_path.display()
                );
                return Ok(FtpFilePreparationResult::DryRunFileFound {
                    total_length: 0,
                    local_path,
                    suffix_path,
                });
            }

            Ok(FtpFilePreparationResult::TotalLengthUnknown {
                local_path,
                suffix_path,
            })
        }
    }
}

// =============================================================================
// Zero-length file handling
// =============================================================================

/// Handle the zero-length file branch of `onFileSizeDetermined()`.
///
/// C++ logic (lines 400-474 of FtpNegotiationCommand.cc):
/// 1. If dry-run → `initPieceStorage()` + `onDryRunFileFound()`
/// 2. If file exists with matching length + knows total length:
///    a. If checksum needed → verify checksum
///    b. Else → mark all pieces done, download already completed
/// 3. Otherwise → adjust filename, init piece storage, create zero-length file
fn handle_zero_length_file(
    config: &FtpFilePreparationConfig,
    local_path: PathBuf,
    suffix_path: String,
) -> Result<FtpFilePreparationResult, Aria2Error> {
    // C++: dry-run + zero-length → initPieceStorage + onDryRunFileFound
    if config.dry_run {
        info!("Dry-run: zero-length file found, path={}", local_path.display());
        return Ok(FtpFilePreparationResult::DryRunFileFound {
            total_length: 0,
            local_path,
            suffix_path,
        });
    }

    // C++: downloadFinishedByFileLength() — file exists on disk with
    // the expected length (0 bytes). This only applies when the total
    // length is known (i.e., SIZE returned 213 with size=0).
    if config.knows_total_length && config.file_exists_with_matching_length {
        if config.checksum_verification_needed {
            info!(
                "Zero-length file exists, checksum verification needed: {}",
                local_path.display()
            );
            // TODO: C++ pushes ChecksumCheckIntegrityEntry and sets
            // sequence_ = SEQ_EXIT. Requires CheckIntegrityManager integration.
            return Ok(FtpFilePreparationResult::ChecksumVerificationNeeded {
                local_path,
            });
        }

        // C++: markAllPiecesDone(), setChecksumVerified(true),
        // sequence_ = SEQ_DOWNLOAD_ALREADY_COMPLETED
        info!(
            "Zero-length file already exists, download complete: {}",
            local_path.display()
        );
        return Ok(FtpFilePreparationResult::DownloadAlreadyCompleted {
            local_path,
        });
    }

    // C++: adjustFilename + initPieceStorage + openFile for zero-length file.
    // Also handles the knows_total_length case where file doesn't exist yet.
    info!(
        "Zero-length file, preparing for download: {}",
        local_path.display()
    );
    Ok(FtpFilePreparationResult::ZeroLengthFile {
        local_path,
        suffix_path,
        dry_run: config.dry_run,
        file_exists_with_matching_length: config.file_exists_with_matching_length,
        checksum_verification_needed: config.checksum_verification_needed,
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- percent_decode ----

    #[test]
    fn test_percent_decode_ascii() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
    }

    #[test]
    fn test_percent_decode_multi_byte_utf8() {
        // Chinese character for "file": 文件 = E6 96 87 E4 BB B6
        assert_eq!(percent_decode("%E6%96%87%E4%BB%B6"), "文件");
    }

    #[test]
    fn test_percent_decode_no_encoding() {
        assert_eq!(percent_decode("file.txt"), "file.txt");
    }

    #[test]
    fn test_percent_decode_invalid_hex() {
        // Invalid hex after % should preserve literal characters
        assert_eq!(percent_decode("foo%ZZbar"), "foo%ZZbar");
    }

    #[test]
    fn test_percent_decode_incomplete_percent() {
        // % at end of string with no hex digits
        assert_eq!(percent_decode("test%"), "test%");
    }

    #[test]
    fn test_percent_decode_single_hex_digit() {
        // % followed by only one hex digit: "test%2" → chars.take(2) on "2" → "2"
        // This is a valid hex byte (0x02), so it decodes to the STX control char.
        // This matches C++ percentDecode which consumes 2 chars greedily.
        let result = percent_decode("test%2");
        assert_eq!(result, "test\u{0002}");
    }

    #[test]
    fn test_percent_decode_mixed() {
        assert_eq!(percent_decode("/pub/my%20file.tar.gz"), "/pub/my file.tar.gz");
    }

    // ---- create_safe_path ----

    #[test]
    fn test_create_safe_path_normal() {
        assert_eq!(create_safe_path("file.txt"), "file.txt");
    }

    #[test]
    fn test_create_safe_path_with_slashes() {
        assert_eq!(create_safe_path("path/to/file.txt"), "path_to_file.txt");
    }

    #[test]
    fn test_create_safe_path_backslash() {
        assert_eq!(create_safe_path("win\\path"), "win_path");
    }

    #[test]
    fn test_create_safe_path_null_byte() {
        assert_eq!(create_safe_path("file\0name"), "filename");
    }

    #[test]
    fn test_create_safe_path_empty() {
        assert_eq!(create_safe_path(""), "index.html");
    }

    #[test]
    fn test_create_safe_path_only_slashes() {
        assert_eq!(create_safe_path("///"), "___");
    }

    // ---- apply_dir ----

    #[test]
    fn test_apply_dir_normal() {
        assert_eq!(
            apply_dir("/downloads", "file.zip"),
            PathBuf::from("/downloads/file.zip")
        );
    }

    #[test]
    fn test_apply_dir_empty() {
        assert_eq!(apply_dir("", "file.zip"), PathBuf::from("file.zip"));
    }

    #[test]
    fn test_apply_dir_dot() {
        assert_eq!(apply_dir(".", "file.zip"), PathBuf::from("file.zip"));
    }

    #[test]
    fn test_apply_dir_trailing_slash() {
        // Path::join handles trailing slashes correctly
        assert_eq!(
            apply_dir("/downloads/", "file.zip"),
            PathBuf::from("/downloads/file.zip")
        );
    }

    // ---- derive_local_path ----

    #[test]
    fn test_derive_local_path_simple() {
        let (path, suffix) = derive_local_path("/downloads", "/pub/file.tar.gz");
        assert_eq!(path, PathBuf::from("/downloads/_pub_file.tar.gz"));
        assert_eq!(suffix, "_pub_file.tar.gz");
    }

    #[test]
    fn test_derive_local_path_encoded() {
        let (path, suffix) = derive_local_path("/tmp", "my%20file.zip");
        assert_eq!(path, PathBuf::from("/tmp/my file.zip"));
        assert_eq!(suffix, "my file.zip");
    }

    #[test]
    fn test_derive_local_path_no_dir() {
        let (path, suffix) = derive_local_path(".", "archive.tar");
        assert_eq!(path, PathBuf::from("archive.tar"));
        assert_eq!(suffix, "archive.tar");
    }

    #[test]
    fn test_derive_local_path_empty_request() {
        let (path, suffix) = derive_local_path("/downloads", "");
        // create_safe_path returns "index.html" for empty input
        assert_eq!(path, PathBuf::from("/downloads/index.html"));
        assert_eq!(suffix, "index.html");
    }

    // ---- prepare_ftp_file: normal download ----

    #[test]
    fn test_prepare_normal_download() {
        let config = FtpFilePreparationConfig {
            dir: "/downloads".to_string(),
            request_file: "/pub/linux/file.tar.gz".to_string(),
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(1024), &config).unwrap();

        match result {
            FtpFilePreparationResult::NormalDownload {
                total_length,
                local_path,
                suffix_path,
            } => {
                assert_eq!(total_length, 1024);
                assert_eq!(suffix_path, "_pub_linux_file.tar.gz");
                assert!(local_path.to_string_lossy().contains("file.tar.gz"));
            }
            _ => panic!("Expected NormalDownload, got {:?}", result),
        }
    }

    #[test]
    fn test_prepare_normal_download_path_already_set() {
        let config = FtpFilePreparationConfig {
            dir: "/downloads".to_string(),
            request_file: "/downloads/override.zip".to_string(),
            path_already_set: true,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(2048), &config).unwrap();

        match result {
            FtpFilePreparationResult::NormalDownload {
                total_length,
                local_path,
                ..
            } => {
                assert_eq!(total_length, 2048);
                assert_eq!(local_path, PathBuf::from("/downloads/override.zip"));
            }
            _ => panic!("Expected NormalDownload, got {:?}", result),
        }
    }

    // ---- prepare_ftp_file: overflow ----

    #[test]
    fn test_prepare_overflow() {
        let config = FtpFilePreparationConfig::default();
        let result = prepare_ftp_file(&FtpFileSize::Known(u64::MAX), &config);

        assert!(result.is_err());
        match result.unwrap_err() {
            Aria2Error::Recoverable(RecoverableError::FtpProtocolError { message }) => {
                assert!(message.contains("too large") || message.contains("exceeds"));
            }
            other => panic!("Expected FtpProtocolError, got {:?}", other),
        }
    }

    #[test]
    fn test_prepare_exact_i64_max_is_ok() {
        // i64::MAX should NOT overflow — C++ checks size > numeric_limits<a2_off_t>::max()
        let config = FtpFilePreparationConfig {
            request_file: "file.dat".to_string(),
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(i64::MAX as u64), &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_prepare_i64_max_plus_one_overflows() {
        let config = FtpFilePreparationConfig {
            request_file: "file.dat".to_string(),
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(i64::MAX as u64 + 1), &config);
        assert!(result.is_err());
    }

    // ---- prepare_ftp_file: zero-length ----

    #[test]
    fn test_prepare_zero_length_dry_run() {
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "empty.dat".to_string(),
            dry_run: true,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(0), &config).unwrap();

        match result {
            FtpFilePreparationResult::DryRunFileFound {
                total_length,
                local_path,
                ..
            } => {
                assert_eq!(total_length, 0);
                assert!(local_path.to_string_lossy().contains("empty.dat"));
            }
            _ => panic!("Expected DryRunFileFound, got {:?}", result),
        }
    }

    #[test]
    fn test_prepare_zero_length_file_exists_no_checksum() {
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "empty.dat".to_string(),
            file_exists_with_matching_length: true,
            knows_total_length: true,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(0), &config).unwrap();

        match result {
            FtpFilePreparationResult::DownloadAlreadyCompleted { local_path } => {
                assert!(local_path.to_string_lossy().contains("empty.dat"));
            }
            _ => panic!("Expected DownloadAlreadyCompleted, got {:?}", result),
        }
    }

    #[test]
    fn test_prepare_zero_length_file_exists_checksum_needed() {
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "empty.dat".to_string(),
            file_exists_with_matching_length: true,
            knows_total_length: true,
            checksum_verification_needed: true,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(0), &config).unwrap();

        match result {
            FtpFilePreparationResult::ChecksumVerificationNeeded { local_path } => {
                assert!(local_path.to_string_lossy().contains("empty.dat"));
            }
            _ => panic!("Expected ChecksumVerificationNeeded, got {:?}", result),
        }
    }

    #[test]
    fn test_prepare_zero_length_file_not_exists() {
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "newfile.dat".to_string(),
            file_exists_with_matching_length: false,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(0), &config).unwrap();

        match result {
            FtpFilePreparationResult::ZeroLengthFile {
                local_path,
                suffix_path,
                dry_run,
                file_exists_with_matching_length,
                checksum_verification_needed,
            } => {
                assert!(local_path.to_string_lossy().contains("newfile.dat"));
                assert_eq!(suffix_path, "newfile.dat");
                assert!(!dry_run);
                assert!(!file_exists_with_matching_length);
                assert!(!checksum_verification_needed);
            }
            _ => panic!("Expected ZeroLengthFile, got {:?}", result),
        }
    }

    #[test]
    fn test_prepare_zero_length_file_not_exists_but_knows_length() {
        // C++ path: knows total length, file doesn't exist, zero-length.
        // After adjustFilename + initPieceStorage + openFile,
        // it hits the "File length becomes zero" check.
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "zero.dat".to_string(),
            file_exists_with_matching_length: false,
            knows_total_length: true,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(0), &config).unwrap();

        // Should return ZeroLengthFile since file doesn't exist yet
        match result {
            FtpFilePreparationResult::ZeroLengthFile { suffix_path, .. } => {
                assert_eq!(suffix_path, "zero.dat");
            }
            _ => panic!("Expected ZeroLengthFile, got {:?}", result),
        }
    }

    // ---- prepare_ftp_file: unknown size ----

    #[test]
    fn test_prepare_unknown_size() {
        let config = FtpFilePreparationConfig {
            dir: "/downloads".to_string(),
            request_file: "/pub/unknown.dat".to_string(),
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Unknown, &config).unwrap();

        match result {
            FtpFilePreparationResult::TotalLengthUnknown {
                local_path,
                suffix_path,
            } => {
                assert!(local_path.to_string_lossy().contains("unknown.dat"));
                assert_eq!(suffix_path, "_pub_unknown.dat");
            }
            _ => panic!("Expected TotalLengthUnknown, got {:?}", result),
        }
    }

    #[test]
    fn test_prepare_unknown_size_dry_run() {
        let config = FtpFilePreparationConfig {
            dir: "/downloads".to_string(),
            request_file: "file.zip".to_string(),
            dry_run: true,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Unknown, &config).unwrap();

        match result {
            FtpFilePreparationResult::DryRunFileFound { total_length, .. } => {
                assert_eq!(total_length, 0);
            }
            _ => panic!("Expected DryRunFileFound, got {:?}", result),
        }
    }

    // ---- prepare_ftp_file: dry-run with non-zero size ----

    #[test]
    fn test_prepare_dry_run_non_zero() {
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "bigfile.iso".to_string(),
            dry_run: true,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(4_700_000_000), &config).unwrap();

        match result {
            FtpFilePreparationResult::DryRunFileFound {
                total_length,
                suffix_path,
                ..
            } => {
                assert_eq!(total_length, 4_700_000_000);
                assert_eq!(suffix_path, "bigfile.iso");
            }
            _ => panic!("Expected DryRunFileFound, got {:?}", result),
        }
    }

    // ---- prepare_ftp_file: piece_storage already initialized ----

    #[test]
    fn test_prepare_piece_storage_already_initialized() {
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "file.bin".to_string(),
            piece_storage_initialized: true,
            ..Default::default()
        };
        // Should not error; just validates and proceeds
        let result = prepare_ftp_file(&FtpFileSize::Known(1024), &config).unwrap();
        match result {
            FtpFilePreparationResult::NormalDownload { total_length, .. } => {
                assert_eq!(total_length, 1024);
            }
            _ => panic!("Expected NormalDownload, got {:?}", result),
        }
    }

    // ---- Edge cases ----

    #[test]
    fn test_prepare_zero_length_file_exists_but_unknown_length() {
        // When total length is unknown (shouldn't happen for Known(0), but
        // tests the config flag independently), the downloadFinishedByFileLength
        // check should not apply.
        let config = FtpFilePreparationConfig {
            dir: "/tmp".to_string(),
            request_file: "file.dat".to_string(),
            file_exists_with_matching_length: true,
            knows_total_length: false,
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(0), &config).unwrap();

        // Since knows_total_length is false, we don't treat the existing
        // zero-length file as "already completed" — we return ZeroLengthFile
        match result {
            FtpFilePreparationResult::ZeroLengthFile {
                file_exists_with_matching_length,
                ..
            } => {
                assert!(file_exists_with_matching_length);
            }
            _ => panic!("Expected ZeroLengthFile, got {:?}", result),
        }
    }

    #[test]
    fn test_prepare_large_but_valid_size() {
        // 100 GiB — should be fine
        let size = 100u64 * 1024 * 1024 * 1024;
        let config = FtpFilePreparationConfig {
            dir: "/data".to_string(),
            request_file: "big.bin".to_string(),
            ..Default::default()
        };
        let result = prepare_ftp_file(&FtpFileSize::Known(size), &config).unwrap();
        match result {
            FtpFilePreparationResult::NormalDownload { total_length, .. } => {
                assert_eq!(total_length, size);
            }
            _ => panic!("Expected NormalDownload"),
        }
    }

    #[test]
    fn test_derive_local_path_url_encoded_path() {
        let (path, suffix) = derive_local_path("/out", "my%20docs%2Freadme.txt");
        // percent decode: "my docs/readme.txt"
        // create_safe_path: "my docs_readme.txt" (/ → _)
        assert_eq!(suffix, "my docs_readme.txt");
        assert_eq!(path, PathBuf::from("/out/my docs_readme.txt"));
    }
}
