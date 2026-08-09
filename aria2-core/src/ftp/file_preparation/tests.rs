//! Tests for FTP file preparation pipeline.

use std::path::PathBuf;

use super::*;

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
    assert_eq!(
        percent_decode("/pub/my%20file.tar.gz"),
        "/pub/my file.tar.gz"
    );
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

#[test]
fn test_derive_local_path_url_encoded_path() {
    let (path, suffix) = derive_local_path("/out", "my%20docs%2Freadme.txt");
    // percent decode: "my docs/readme.txt"
    // create_safe_path: "my docs_readme.txt" (/ → _)
    assert_eq!(suffix, "my docs_readme.txt");
    assert_eq!(path, PathBuf::from("/out/my docs_readme.txt"));
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
