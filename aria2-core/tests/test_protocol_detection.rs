use aria2_core::validation::protocol_detector::{detect, InputType, DetectedInput};

#[test]
fn test_detect_http_url() {
    let d = detect("http://example.com/file.zip").unwrap();
    assert_eq!(d.input_type, InputType::HttpUrl);
    assert_eq!(d.raw, "http://example.com/file.zip");
    assert!(d.file_data.is_none());
}

#[test]
fn test_detect_https_url() {
    let d = detect("https://cdn.example.com/file.iso").unwrap();
    assert_eq!(d.input_type, InputType::HttpUrl);
}

#[test]
fn test_detect_ftp_url() {
    let d = detect("ftp://server/path/file.bin").unwrap();
    assert_eq!(d.input_type, InputType::FtpUrl);
}

#[test]
fn test_detect_sftp_url() {
    let d = detect("sftp://user@host:22/path").unwrap();
    assert_eq!(d.input_type, InputType::SftpUrl);
}

#[test]
fn test_detect_magnet_link() {
    let d = detect("magnet:?xt=urn:btih:abc123def456&dn=Ubuntu&tr=udp://tracker.example.com:1337/announce").unwrap();
    assert_eq!(d.input_type, InputType::MagnetLink);
    assert!(d.file_data.is_none());
}

#[test]
fn test_detect_magnet_short_form() {
    let d = detect("magnet?xt=urn:btih:test").unwrap();
    assert_eq!(d.input_type, InputType::MagnetLink);
}

#[test]
fn test_detect_empty_input() {
    assert!(detect("").is_err());
    assert!(detect("   ").is_err());
}

#[test]
fn test_input_type_display() {
    assert_eq!(InputType::HttpUrl.to_string(), "http/https");
    assert_eq!(InputType::FtpUrl.to_string(), "ftp");
    assert_eq!(InputType::SftpUrl.to_string(), "sftp");
    assert_eq!(InputType::TorrentFile.to_string(), "torrent");
    assert_eq!(InputType::MetalinkFile.to_string(), "metalink");
    assert_eq!(InputType::MagnetLink.to_string(), "magnet");
}

#[test]
fn test_detect_torrent_file_by_extension_and_content() {
    use std::io::Write as IoWrite;
    let dir = tempfile::tempdir().unwrap();
    let torrent_path = dir.path().join("test.torrent");

    let fake_torrent = b"d8:announce40:http://tracker.example.com/announce4:info6:lengthi1000e12:piece lengthi32768e6:pieces20:00000000000000000000000ee";
    let mut f = std::fs::File::create(&torrent_path).unwrap();
    f.write_all(fake_torrent).unwrap();

    let d = detect(torrent_path.to_str().unwrap()).unwrap();
    assert_eq!(d.input_type, InputType::TorrentFile);
    assert!(d.file_data.is_some());
    let data = d.file_data.unwrap();
    assert!(data.starts_with(b"d8:"));
}

#[test]
fn test_detect_torrent_file_invalid_content() {
    let dir = tempfile::tempdir().unwrap();
    let bad_path = dir.path().join("bad.torrent");
    std::fs::write(&bad_path, "this is not a valid torrent file content").unwrap();

    let result = detect(bad_path.to_str().unwrap());
    assert!(result.is_err(), "Invalid BEncode content should be rejected");
}

#[test]
fn test_detect_metalink_file_by_extension_and_content() {
    let dir = tempfile::tempdir().unwrap();
    let metalink_path = dir.path().join("test.metalink");

    let metalink_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="test.bin">
      <size>1024</size>
      <url priority="1">http://example.com/test.bin</url>
    </file>
  </files>
</metalink>"#;
    std::fs::write(&metalink_path, metalink_xml).unwrap();

    let d = detect(metalink_path.to_str().unwrap()).unwrap();
    assert_eq!(d.input_type, InputType::MetalinkFile);
    assert!(d.file_data.is_some());
}

#[test]
fn test_detect_metalink_meta4_extension() {
    let dir = tempfile::tempdir().unwrap();
    let meta4_path = dir.path().join("test.meta4");
    let metalink_xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><files><file name="f"><size>10</size><url>http://x.com/f</url></file></files></metalink>"#;
    std::fs::write(&meta4_path, metalink_xml).unwrap();

    let d = detect(meta4_path.to_str().unwrap()).unwrap();
    assert_eq!(d.input_type, InputType::MetalinkFile);
}

#[test]
fn test_detect_unknown_scheme_returns_http_fallback() {
    let d = detect("example.com/file.zip").unwrap();
    assert_eq!(d.input_type, InputType::HttpUrl);
    assert_eq!(d.raw, "http://example.com/file.zip");
}

#[test]
fn test_detect_all_schemes_case_insensitive() {
    assert_eq!(detect("HTTP://X.COM/F").unwrap().input_type, InputType::HttpUrl);
    assert_eq!(detect("HTTPS://X.COM/F").unwrap().input_type, InputType::HttpUrl);
    assert_eq!(detect("FTP://X.COM/F").unwrap().input_type, InputType::FtpUrl);
    assert_eq!(detect("SFTP://X.COM/F").unwrap().input_type, InputType::SftpUrl);
    assert_eq!(detect("MAGNET:?XT=URN:BTHI:ABC").unwrap().input_type, InputType::MagnetLink);
}
