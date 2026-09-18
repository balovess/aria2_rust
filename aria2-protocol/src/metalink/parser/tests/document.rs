use super::super::parse::detect_dir_traversal;
use super::super::*;
use crate::metalink::resource::ResourceType;

fn make_v3_metalink() -> Vec<u8> {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="http://www.metalinker.org/">
  <files>
<file name="test.iso">
  <size>1048576</size>
  <identity>abc123def456</identity>
  <verification>
    <hash type="sha-256">e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855</hash>
    <hash type="sha-1">da39a3ee5e6b4b0d3255bfef95601890afd80709</hash>
  </verification>
  <resources maxconnections="4">
    <url type="http" location="cn" preference="90">http://mirror1.cn/test.iso</url>
    <url type="http" location="us" preference="80">http://mirror2.us/test.iso</url>
  </resources>
</file>
  </files>
</metalink>"#
        .as_bytes()
        .to_vec()
}

fn make_v4_metalink() -> Vec<u8> {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <generator>test-generator/1.0</generator>
  <origin>Dynamic</origin>
  <published>2024-01-01T00:00:00Z</published>
  <file name="example.bin">
<size>2048576</size>
<identity>fedcba654321</identity>
<hash type="sha-256">cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2baff41</hash>
<url priority="1">http://primary.example.com/example.bin</url>
<url priority="50">http://backup.example.com/example.bin</url>
<pieces length="262144" type="sha-256">hash1hash2</pieces>
  </file>
</metalink>"#.as_bytes().to_vec()
}

#[test]
fn test_parse_v3_metalink() {
    let data = make_v3_metalink();
    let doc = MetalinkDocument::parse(&data, None).unwrap();
    assert_eq!(doc.version, MetalinkVersion::V3);
    assert_eq!(doc.files.len(), 1);
    assert_eq!(doc.files[0].name, "test.iso");
    assert_eq!(doc.files[0].size, Some(1048576));
    // V3 uses <verification><hash> and <resources><url>
    assert_eq!(doc.files[0].urls.len(), 2);
    assert_eq!(doc.files[0].hashes.len(), 2);
    // V3 meta_urls: no <metaurl> in this V3 fixture
    assert_eq!(doc.files[0].meta_urls.len(), 0);
    // V3 preference=90 → priority = 101 - 90 = 11
    // V3 preference=80 → priority = 101 - 80 = 21
    assert_eq!(doc.files[0].urls[0].preference, Some(90));
    assert_eq!(doc.files[0].urls[0].priority, 11);
    assert_eq!(doc.files[0].urls[1].preference, Some(80));
    assert_eq!(doc.files[0].urls[1].priority, 21);
}

#[test]
fn test_parse_v4_metalink() {
    let data = make_v4_metalink();
    let doc = MetalinkDocument::parse(&data, None).unwrap();
    assert_eq!(doc.version, MetalinkVersion::V4);
    assert_eq!(doc.origin.as_deref(), Some("Dynamic"));
    assert_eq!(doc.published.as_deref(), Some("2024-01-01T00:00:00Z"));
    assert_eq!(doc.files[0].name, "example.bin");
    assert_eq!(doc.files[0].urls[0].priority, 1);
    assert_eq!(doc.files[0].urls[1].priority, 50);
}

#[test]
fn test_url_sorting() {
    let data = make_v3_metalink();
    let doc = MetalinkDocument::parse(&data, None).unwrap();
    let urls = doc.files[0].get_sorted_urls();
    // V3 preference=90 → priority=11, preference=80 → priority=21
    // Lower priority value = tried first (V4 semantics)
    assert_eq!(urls[0].priority, 11);
    assert_eq!(urls[1].priority, 21);
}

#[test]
fn test_preferred_url() {
    let data = make_v3_metalink();
    let doc = MetalinkDocument::parse(&data, None).unwrap();
    let preferred = doc.files[0].get_preferred_url();
    assert!(preferred.is_some());
    // V3 preference=90 → priority=11 (best)
    assert_eq!(preferred.unwrap().priority, 11);
}

#[test]
fn test_hash_algorithm_parsing() {
    assert_eq!(HashAlgorithm::parse("md5"), Some(HashAlgorithm::Md5));
    assert_eq!(HashAlgorithm::parse("SHA-256"), Some(HashAlgorithm::Sha256));
    assert_eq!(HashAlgorithm::parse("sha224"), Some(HashAlgorithm::Sha224));
    assert_eq!(HashAlgorithm::parse("SHA-384"), Some(HashAlgorithm::Sha384));
    assert_eq!(HashAlgorithm::parse("sha512"), Some(HashAlgorithm::Sha512));
    assert_eq!(HashAlgorithm::parse("unknown"), None);
    assert_eq!(HashAlgorithm::Md5.hash_len(), 32);
    assert_eq!(HashAlgorithm::Sha224.hash_len(), 56);
    assert_eq!(HashAlgorithm::Sha256.hash_len(), 64);
    assert_eq!(HashAlgorithm::Sha384.hash_len(), 96);
}

#[test]
fn test_mediatype_detection() {
    assert!(MediaType::parse("torrent").is_torrent());
    assert!(MediaType::parse("application/x-bittorrent").is_torrent());
    assert!(!MediaType::parse("xml").is_torrent());
}

#[test]
fn test_empty_metalink_fails() {
    let bad = b"<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"></metalink>".to_vec();
    assert!(MetalinkDocument::parse(&bad, None).is_err());
}

#[test]
fn test_single_file_accessor() {
    let data = make_v3_metalink();
    let doc = MetalinkDocument::parse(&data, None).unwrap();
    assert!(doc.single_file().is_some());
    assert_eq!(doc.single_file().unwrap().name, "test.iso");
}

#[test]
fn test_pieces_info() {
    let data = make_v4_metalink();
    let doc = MetalinkDocument::parse(&data, None).unwrap();
    let pieces = &doc.files[0].pieces;
    assert!(pieces.is_some());
    let p = pieces.as_ref().unwrap();
    assert_eq!(p.length, 262144);
    assert_eq!(p.type_, HashAlgorithm::Sha256);
    // "hash1hash2" is 10 chars < 64: no complete sha-256 digest, so the
    // text-mode chunker must drop it entirely (count 0, not garbage).
    assert_eq!(p.piece_count(), 0);
}

#[test]
fn test_pieces_concatenated_text_is_chunked() {
    // Two real 64-char sha-256 digests concatenated with no whitespace.
    let h1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let h2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="f.bin">
<size>524288</size>
<pieces length="262144" type="sha-256">{h1}{h2}</pieces>
  </file>
</metalink>"#
    );
    let doc = MetalinkDocument::parse(xml.as_bytes(), None).unwrap();
    let p = doc.files[0].pieces.as_ref().unwrap();
    assert_eq!(
        p.piece_count(),
        2,
        "contiguous digests must be chunked by hex length"
    );
    assert_eq!(p.hashes[0], h1);
    assert_eq!(p.hashes[1], h2);
    assert_eq!(p.num_pieces(524288), 2);
}

#[test]
fn test_pieces_v4_hash_children() {
    // V4 spec: `<pieces>` contains one `<hash>` element per piece.
    let h1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let h2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="f.bin">
<size>524288</size>
<pieces length="262144" type="sha-256">
  <hash>{h1}</hash>
  <hash>{h2}</hash>
</pieces>
  </file>
</metalink>"#
    );
    let doc = MetalinkDocument::parse(xml.as_bytes(), None).unwrap();
    let p = doc.files[0].pieces.as_ref().unwrap();
    assert_eq!(
        p.piece_count(),
        2,
        "v4 <hash> children must be collected per piece"
    );
    assert_eq!(p.hashes[0], h1);
    assert_eq!(p.hashes[1], h2);
    assert_eq!(p.length, 262144);
    assert_eq!(p.type_, HashAlgorithm::Sha256);
    // Verification hashes (whole-file) must NOT be polluted by pieces.
    assert!(doc.files[0].hashes.is_empty());
}

#[test]
fn test_all_urls_collector() {
    let data = make_v3_metalink();
    let doc = MetalinkDocument::parse(&data, None).unwrap();
    let urls = doc.all_urls();
    assert_eq!(urls.len(), 2);
}

#[test]
fn test_dir_traversal_detection() {
    assert!(detect_dir_traversal(".."));
    assert!(detect_dir_traversal("."));
    assert!(detect_dir_traversal("../etc/passwd"));
    assert!(detect_dir_traversal("./secret"));
    assert!(detect_dir_traversal("/etc/passwd"));
    assert!(detect_dir_traversal("foo/../bar"));
    assert!(detect_dir_traversal("foo/./bar"));
    assert!(detect_dir_traversal("foo/"));
    assert!(detect_dir_traversal("foo/."));
    assert!(detect_dir_traversal("foo/.."));
    assert!(!detect_dir_traversal("normal.txt"));
    assert!(!detect_dir_traversal("path/to/file.iso"));
    assert!(!detect_dir_traversal(""));
}

#[test]
fn test_dir_traversal_in_metalink_filename() {
    let bad = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="../../../etc/passwd">
<size>100</size>
<url priority="1">http://example.com/file</url>
  </file>
</metalink>"#
        .as_bytes()
        .to_vec();
    let doc = MetalinkDocument::parse(&bad, None).unwrap();
    // Directory traversal name should be rejected and replaced
    assert_ne!(doc.files[0].name, "../../../etc/passwd");
    assert!(doc.files[0].name.starts_with("unknown_"));
}

#[test]
fn test_namespace_detection_v3() {
    let v3_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="http://www.metalinker.org/">
  <files>
<file name="test.iso">
  <size>100</size>
  <url type="http" preference="50">http://example.com/test.iso</url>
</file>
  </files>
</metalink>"#
        .as_bytes()
        .to_vec();
    let doc = MetalinkDocument::parse(&v3_xml, None).unwrap();
    assert_eq!(doc.version, MetalinkVersion::V3);
}

#[test]
fn test_namespace_detection_v4() {
    let v4_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="test.bin">
<size>100</size>
<url priority="1">http://example.com/test.bin</url>
  </file>
</metalink>"#
        .as_bytes()
        .to_vec();
    let doc = MetalinkDocument::parse(&v4_xml, None).unwrap();
    assert_eq!(doc.version, MetalinkVersion::V4);
}

// ========================================================================
// ResourceType + V3 type attribute parsing tests
// ========================================================================

#[test]
fn test_resource_type_from_url_type_str() {
    assert_eq!(ResourceType::from_url_type_str("http"), ResourceType::Http);
    assert_eq!(
        ResourceType::from_url_type_str("HTTPS"),
        ResourceType::Https
    );
    assert_eq!(ResourceType::from_url_type_str("ftp"), ResourceType::Ftp);
    assert_eq!(
        ResourceType::from_url_type_str("bittorrent"),
        ResourceType::BitTorrent
    );
    // Unknown type strings map to NotSupported per C++ MetalinkParserController::setTypeOfResource()
    assert_eq!(
        ResourceType::from_url_type_str("unknown"),
        ResourceType::NotSupported
    );
}

#[test]
fn test_resource_type_as_str() {
    assert_eq!(ResourceType::Ftp.as_str(), "ftp");
    assert_eq!(ResourceType::Http.as_str(), "http");
    assert_eq!(ResourceType::Https.as_str(), "https");
    assert_eq!(ResourceType::BitTorrent.as_str(), "bittorrent");
    assert_eq!(ResourceType::NotSupported.as_str(), "not_supported");
    assert_eq!(ResourceType::Unknown.as_str(), "unknown");
}

#[test]
fn test_v3_type_attribute_parsed_into_resource_type() {
    let v3_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="http://www.metalinker.org/">
  <files>
<file name="test.iso">
  <size>1048576</size>
  <resources>
    <url type="http" location="cn" preference="90">http://mirror1.cn/test.iso</url>
    <url type="https" location="us" preference="80">https://mirror2.us/test.iso</url>
    <url type="ftp" location="jp" preference="70">ftp://mirror3.jp/test.iso</url>
    <url type="bittorrent" preference="50">magnet:?xt=urn:btih:abc</url>
    <url type="unknown" preference="40">http://mirror5.unknown/test.iso</url>
  </resources>
</file>
  </files>
</metalink>"#
        .as_bytes()
        .to_vec();
    let doc = MetalinkDocument::parse(&v3_xml, None).unwrap();
    let urls = &doc.files[0].urls;
    assert_eq!(urls.len(), 5);
    // V3 type="http" should override URL-scheme auto-detection
    assert_eq!(urls[0].resource_type, ResourceType::Http);
    assert_eq!(urls[1].resource_type, ResourceType::Https);
    assert_eq!(urls[2].resource_type, ResourceType::Ftp);
    assert_eq!(urls[3].resource_type, ResourceType::BitTorrent);
    // Unknown type strings → NotSupported per C++ behavior
    assert_eq!(urls[4].resource_type, ResourceType::NotSupported);
}

#[test]
fn test_v4_url_auto_detects_resource_type() {
    // V4 has no type attribute; resource_type is auto-detected from URL scheme
    let v4_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="test.bin">
<size>100</size>
<url priority="1">http://example.com/test.bin</url>
<url priority="2">https://example.com/test.bin</url>
<url priority="3">ftp://example.com/test.bin</url>
  </file>
</metalink>"#
        .as_bytes()
        .to_vec();
    let doc = MetalinkDocument::parse(&v4_xml, None).unwrap();
    assert_eq!(doc.files[0].urls[0].resource_type, ResourceType::Http);
    assert_eq!(doc.files[0].urls[1].resource_type, ResourceType::Https);
    assert_eq!(doc.files[0].urls[2].resource_type, ResourceType::Ftp);
}
