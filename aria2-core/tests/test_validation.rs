use aria2_core::validation::uri::{validate, is_magnet_link, is_torrent_file, sanitize_filename_from_uri};
use aria2_core::validation::option::*;
use aria2_core::validation::filename::{sanitize, make_unique};

#[test]
fn test_validate_http_uri() {
    let result = validate("http://example.com/file.zip");
    assert!(result.is_ok());
    let v = result.unwrap();
    assert_eq!(v.scheme, "http");
    assert!(!v.is_magnet);
}

#[test]
fn test_validate_https_uri() {
    let result = validate("https://cdn.example.com/data.bin");
    assert!(result.is_ok());
    let v = result.unwrap();
    assert_eq!(v.scheme, "https");
}

#[test]
fn test_validate_ftp_uri() {
    let result = validate("ftp://files.example.com/archive.tar.gz");
    assert!(result.is_ok());
    assert_eq!(result.unwrap().scheme, "ftp");
}

#[test]
fn test_validate_sftp_uri() {
    let result = validate("sftp://user@server.com/path/file.txt");
    assert!(result.is_ok());
    assert_eq!(result.unwrap().scheme, "sftp");
}

#[test]
fn test_validate_magnet_link() {
    let result = validate("magnet:?xt=urn:btih:abc123");
    assert!(result.is_ok());
    let v = result.unwrap();
    assert!(v.is_magnet);
    assert_eq!(v.scheme, "magnet");
}

#[test]
fn test_validate_file_uri_torrent() {
    let result = validate("file:///home/user/downloads/test.torrent");
    assert!(result.is_ok());
    let v = result.unwrap();
    assert!(v.is_torrent);
}

#[test]
fn test_reject_empty_uri() {
    assert!(validate("").is_err());
    assert!(validate("   ").is_err());
}

#[test]
fn test_reject_no_scheme() {
    assert!(validate("example.com/file").is_err());
    assert!(validate("/path/to/file").is_err());
}

#[test]
fn test_reject_dangerous_scheme() {
    assert!(validate("javascript:alert(1)").is_err());
    assert!(validate("data:text/html,<h1>hi</h1>").is_err());
}

#[test]
fn test_reject_unsupported_scheme() {
    assert!(validate("ssh://server.com/file").is_err());
}

#[test]
fn test_is_magnet_link() {
    assert!(is_magnet_link("magnet:?xt=urn:btih:abc"));
    assert!(is_magnet_link("magnet?xt=urn:btih:abc"));
    assert!(!is_magnet_link("http://example.com"));
}

#[test]
fn test_is_torrent_file() {
    assert!(is_torrent_file("/path/to/file.torrent"));
    assert!(is_torrent_file("file.torrent"));
    assert!(!is_torrent_file("/path/to/file.zip"));
}

#[test]
fn test_sanitize_filename_from_uri() {
    let name = sanitize_filename_from_uri("http://example.com/path/to/my%20file.zip");
    assert!(name.contains("my file.zip"));
    assert!(!name.contains("../"));
}

#[test]
fn test_sanitize_filename_traversal_removed() {
    let name = sanitize_filename_from_uri("http://evil.com/../../../etc/passwd");
    assert!(!name.contains(".."));
}

#[test]
fn test_sanitize_filename_long_truncated() {
    let long_name = "a".repeat(300);
    let sanitized = sanitize_filename_from_uri(&format!("http://ex.com/{}", long_name));
    assert!(sanitized.len() <= 255);
}

#[test]
fn test_validate_dir_path_valid() {
    let result = validate_dir_path("/tmp/downloads");
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), std::path::PathBuf::from("/tmp/downloads"));
}

#[test]
fn test_validate_dir_path_empty() {
    assert!(validate_dir_path("").is_err());
}

#[test]
fn test_validate_dir_path_null_byte() {
    assert!(validate_dir_path("/tmp\0evil").is_err());
}

#[test]
fn test_validate_dir_path_too_long() {
    let long = "a".repeat(5000);
    assert!(validate_dir_path(&long).is_err());
}

#[test]
fn test_validate_out_filename_valid() {
    assert!(validate_out_filename("myfile.txt").is_ok());
    assert!(validate_out_filename("data_2024-01.csv").is_ok());
}

#[test]
fn test_validate_out_filename_empty() {
    assert!(validate_out_filename("").is_err());
}

#[test]
fn test_validate_out_filename_illegal_chars() {
    assert!(validate_out_filename("file<name>.txt").is_err());
    assert!(validate_out_filename("file\"name\".txt").is_err());
    assert!(validate_out_filename("file|name").is_err());
}

#[test]
fn test_validate_out_filename_windows_reserved() {
    assert!(validate_out_filename("CON").is_err());
    assert!(validate_out_filename("con").is_err());
    assert!(validate_out_filename("AUX.txt").is_err());
    assert!(validate_out_filename("LPT1").is_err());
}

#[test]
fn test_validate_split_value_valid() {
    assert_eq!(validate_split_value(1).unwrap(), 1);
    assert_eq!(validate_split_value(8).unwrap(), 8);
    assert_eq!(validate_split_value(16).unwrap(), 16);
}

#[test]
fn test_validate_split_value_invalid() {
    assert!(validate_split_value(0).is_err());
    assert!(validate_split_value(17).is_err());
    assert!(validate_split_value(100).is_err());
}

#[test]
fn test_validate_connection_limit_valid() {
    assert_eq!(validate_connection_limit(1).unwrap(), 1);
    assert_eq!(validate_connection_limit(16).unwrap(), 16);
}

#[test]
fn test_validate_connection_limit_invalid() {
    assert!(validate_connection_limit(0).is_err());
    assert!(validate_connection_limit(20).is_err());
}

#[test]
fn test_sanitize_control_chars() {
    let result = sanitize("file\x00name\x1ftest");
    assert!(!result.contains('\x00'));
    assert!(!result.contains('\x1f'));
    assert!(result.contains('_'));
}

#[test]
fn test_sanitize_illegal_chars() {
    let result = sanitize("file<>:\"/\\|?*name");
    for c in ['<', '>', ':', '"', '/', '\\', '|', '?', '*'] {
        assert!(!result.contains(c));
    }
}

#[test]
fn test_sanitize_empty_returns_fallback() {
    assert_eq!(sanitize(""), "download");
}

#[test]
fn test_sanitize_trim_dots_and_spaces() {
    assert_eq!(sanitize("  .hidden.  "), "hidden");
}

#[tokio::test]
async fn test_make_unique_no_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_unique(dir.path(), "newfile.txt");
    assert_eq!(path.file_name().unwrap(), "newfile.txt");
}

#[tokio::test]
async fn test_make_unique_with_conflict() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "").unwrap();
    let path = make_unique(dir.path(), "existing.txt");
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(name.starts_with("existing"));
    assert_ne!(name, "existing.txt");
}
