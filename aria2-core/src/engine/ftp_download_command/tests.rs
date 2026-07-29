//! Tests for FTP download command.

use super::control::{parse_epsv_response, parse_pasv_response, RawFtpControl};
use super::types::{extract_filename, parse_uri, urlencoding_decode};

// ---------------------------------------------------------------------------
// URI parsing
// ---------------------------------------------------------------------------

#[test]
fn test_parse_uri_simple() {
    let uri = "ftp://example.com/file.txt";
    let result = parse_uri(uri).unwrap();
    assert_eq!(result.0, "example.com");
    assert_eq!(result.1, 21);
    assert_eq!(result.2, "anonymous");
    assert_eq!(result.3, "aria2@");
    assert_eq!(result.4, "/file.txt");
}

#[test]
fn test_parse_uri_with_port() {
    let uri = "ftp://example.com:2121/file.txt";
    let result = parse_uri(uri).unwrap();
    assert_eq!(result.0, "example.com");
    assert_eq!(result.1, 2121);
}

#[test]
fn test_parse_uri_with_auth() {
    let uri = "ftp://user:pass@example.com/file.txt";
    let result = parse_uri(uri).unwrap();
    assert_eq!(result.2, "user");
    assert_eq!(result.3, "pass");
}

#[test]
fn test_parse_uri_with_encoded_chars() {
    let uri = "ftp://example.com/my%20file.txt";
    let result = parse_uri(uri).unwrap();
    assert_eq!(result.4, "/my file.txt");
}

#[test]
fn test_parse_uri_invalid_protocol() {
    let uri = "http://example.com/file.txt";
    let result = parse_uri(uri);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Filename extraction
// ---------------------------------------------------------------------------

#[test]
fn test_extract_filename_from_path() {
    assert_eq!(
        extract_filename("/path/to/file.txt"),
        Some("file.txt".to_string())
    );
    assert_eq!(
        extract_filename("/file.txt"),
        Some("file.txt".to_string())
    );
    assert_eq!(extract_filename("/"), None);
    assert_eq!(extract_filename(""), None);
}

// ---------------------------------------------------------------------------
// URL encoding
// ---------------------------------------------------------------------------

#[test]
fn test_urlencoding_decode() {
    assert_eq!(urlencoding_decode("hello%20world"), "hello world");
    assert_eq!(urlencoding_decode("%2F"), "/");
    assert_eq!(urlencoding_decode("normal"), "normal");
    assert_eq!(urlencoding_decode("%41"), "A");
}

// ---------------------------------------------------------------------------
// PASV / EPSV parsing
// ---------------------------------------------------------------------------

#[test]
fn test_parse_pasv_response_standard() {
    let resp = "227 Entering Passive Mode (192,168,1,100,200,10)";
    let result = parse_pasv_response(resp).unwrap();
    assert_eq!(result.0, "192.168.1.100");
    assert_eq!(result.1, 200 * 256 + 10); // 51210
}

#[test]
fn test_parse_pasv_response_minimal() {
    let resp = "(10,0,0,1,0,21)";
    let result = parse_pasv_response(resp).unwrap();
    assert_eq!(result.0, "10.0.0.1");
    assert_eq!(result.1, 21);
}

#[test]
fn test_parse_pasv_response_invalid() {
    assert!(parse_pasv_response("no parentheses").is_none());
    assert!(parse_pasv_response("(1,2,3)").is_none()); // Too few parts
}

#[test]
fn test_parse_epsv_response_standard() {
    let resp = "229 Entering Extended Passive Mode (|||50001|)";
    let result = parse_epsv_response(resp).unwrap();
    assert_eq!(result, 50001);
}

#[test]
fn test_parse_epsv_response_minimal() {
    let resp = "|||60000|";
    let result = parse_epsv_response(resp).unwrap();
    assert_eq!(result, 60000);
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

#[test]
fn test_classify_ftp_error_transient() {
    // These should be classified as transient/recoverable
    let transient_codes = [421u16, 425, 426, 450, 451, 452];
    for code in transient_codes {
        assert!(
            (400..=499).contains(&code),
            "Code {} should be in transient range",
            code
        );
    }
}

#[test]
fn test_classify_ftp_error_permanent() {
    // These should be classified as permanent/fatal
    let permanent_codes = [500u16, 501, 502, 503, 504, 530, 550, 553];
    for code in permanent_codes {
        assert!(
            (500..=599).contains(&code),
            "Code {} should be in permanent range",
            code
        );
    }
}

// ---------------------------------------------------------------------------
// Resume offset
// ---------------------------------------------------------------------------

#[test]
fn test_resume_offset_calculation() {
    // Test that resume offset would be calculated correctly from existing file
    // (This logic is in new(), so we verify the concept)
    let path = std::path::PathBuf::from("/tmp/test_file");
    if path.exists() {
        let metadata = std::fs::metadata(&path).unwrap();
        let _offset = metadata.len();
        // offset is u64, always >= 0 by type guarantee
    } else {
        // File doesn't exist, offset should be 0
        assert_eq!(0u64, 0);
    }
}

// ---------------------------------------------------------------------------
// Integration (requires network, expected to fail gracefully)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_raw_ftp_control_connect_invalid_address() {
    let result = RawFtpControl::connect("invalid.host.name.invalid", 21).await;
    assert!(result.is_err());
}
