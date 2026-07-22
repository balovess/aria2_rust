//! Unit tests for FTP connection module

use super::types::{FtpClient, FtpFileInfo, FtpMode, FtpResponse};

#[test]
fn test_ftp_response_checks() {
    // Test positive completion response (2xx)
    let ok = FtpResponse {
        code: 226,
        message: "Transfer complete".into(),
    };
    assert!(ok.is_success());
    assert!(ok.is_positive_completion());
    assert!(!ok.is_positive_preliminary());

    // Test positive preliminary response (1xx)
    let preliminary = FtpResponse {
        code: 150,
        message: "Opening data connection".into(),
    };
    assert!(preliminary.is_success());
    assert!(!preliminary.is_positive_completion());
    assert!(preliminary.is_positive_preliminary());

    // Test error response (4xx/5xx)
    let error = FtpResponse {
        code: 550,
        message: "File not found".into(),
    };
    assert!(!error.is_success());
    assert!(!error.is_positive_completion());
    assert!(!error.is_positive_preliminary());
}

#[test]
fn test_parse_pasv_response_valid() {
    let msg = "Entering Passive Mode (192,168,1,100,195,123)";
    let result = FtpClient::parse_pasv_response(msg);
    assert!(result.is_ok());
    let (host, port) = result.unwrap();
    assert_eq!(host, "192.168.1.100");
    assert_eq!(port, 195 * 256 + 123); // 195*256 + 123 = 50043
}

#[test]
fn test_parse_pasv_response_invalid() {
    // Missing parentheses
    let msg = "Entering Passive Mode 192,168,1,100,195,123";
    let result = FtpClient::parse_pasv_response(msg);
    assert!(result.is_err());

    // Incorrect number of parts
    let msg2 = "Entering Passive Mode (192,168,1,100,195)";
    let result2 = FtpClient::parse_pasv_response(msg2);
    assert!(result2.is_err());
}

#[test]
fn test_parse_epsv_response_valid() {
    let msg = "Entering Extended Passive Mode (|||50001|)";
    let result = FtpClient::parse_epsv_response(msg);
    assert_eq!(result, Some(50001));
}

#[test]
fn test_parse_epsv_response_invalid() {
    let msg = "Invalid EPSV response";
    let result = FtpClient::parse_epsv_response(msg);
    assert_eq!(result, None);
}

#[test]
fn test_parse_list_line_unix_regular_file() {
    let line = "-rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "document.pdf");
    assert_eq!(info.size, 12345);
    assert!(!info.is_dir);
}

#[test]
fn test_parse_list_line_unix_directory() {
    let line = "drwxr-xr-x  2 user staff   4096 Feb  3 14:20 my_folder";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "my_folder");
    assert_eq!(info.size, 4096);
    assert!(info.is_dir);
}

#[test]
fn test_parse_list_line_unix_symlink() {
    let line = "lrwxrwxrwx  1 user staff      8 Mar 10 09:00 link.txt -> target.txt";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "link.txt"); // Symlink should return link name, not target
    assert!(!info.is_dir);
}

#[test]
fn test_parse_list_line_unix_hidden_file() {
    let line = "-rw-r--r--  1 user staff    512 Apr  1 08:00 .bashrc";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, ".bashrc");
    assert_eq!(info.size, 512);
    assert!(!info.is_dir);
}

#[test]
fn test_parse_list_line_unix_special_entries() {
    // "." and ".." should be ignored
    let dot = "drwxr-xr-x  2 user staff   4096 Jan  1 00:00 .";
    let dotdot = "drwxr-xr-x  2 user staff   4096 Jan  1 00:00 ..";

    assert!(FtpClient::parse_list_line(dot).is_none());
    assert!(FtpClient::parse_list_line(dotdot).is_none());
}

#[test]
fn test_parse_list_line_windows_file() {
    let line = "01-15-24  10:30AM    12345 document.pdf";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "document.pdf");
    assert_eq!(info.size, 12345);
    assert!(!info.is_dir);
}

#[test]
fn test_parse_list_line_windows_directory() {
    let line = "02-03-24  02:20PM    <DIR> my_folder";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "my_folder");
    assert!(info.is_dir);
}

#[test]
fn test_parse_list_line_mlsd_format() {
    let line = "type=file;size=12345;modify=20240115103000;unix.mode=0644; document.pdf";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "document.pdf");
    assert_eq!(info.size, 12345);
    assert!(!info.is_dir);
}

#[test]
fn test_parse_list_line_mlsd_directory() {
    let line = "type=dir;size=4096;modify=20240203142000;unix.mode=0755; my_folder";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "my_folder");
    assert_eq!(info.size, 4096);
    assert!(info.is_dir);
}

#[test]
fn test_ftp_mode_default() {
    let mode = FtpMode::default();
    assert_eq!(mode, FtpMode::Passive);
}

#[test]
fn test_ftp_file_info_creation() {
    let info = FtpFileInfo {
        name: "test.txt".to_string(),
        size: 1024,
        is_dir: false,
    };
    assert_eq!(info.name, "test.txt");
    assert_eq!(info.size, 1024);
    assert!(!info.is_dir);
}

#[test]
fn test_parse_list_line_with_spaces_in_name() {
    // Unix format, filename contains spaces
    let line = "-rw-r--r--  1 user staff   5678 Jan 20 11:00 my document with spaces.txt";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_some());
    let info = result.unwrap();
    assert_eq!(info.name, "my document with spaces.txt");
    assert_eq!(info.size, 5678);
}

#[test]
fn test_parse_list_line_unrecognized_format() {
    // Unrecognized format
    let line = "this is not a valid listing format";
    let result = FtpClient::parse_list_line(line);
    assert!(result.is_none());
}

#[test]
fn test_parse_pasv_edge_cases() {
    // Edge case: minimum port
    let min_msg = "Entering Passive Mode (127,0,0,1,0,0)";
    let min_result = FtpClient::parse_pasv_response(min_msg).unwrap();
    assert_eq!(min_result.1, 0);

    // Edge case: maximum port
    let max_msg = "Entering Passive Mode (255,255,255,255,255,255)";
    let max_result = FtpClient::parse_pasv_response(max_msg).unwrap();
    assert_eq!(max_result.1, 255 * 256 + 255); // 65535
}
