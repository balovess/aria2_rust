//! Tests for Metalink/HTTP parsing.

use super::helpers::{split_link_entries, unquote};
use super::parser::{MetalinkHttpParser, deduplicate_digests};
use super::types::MetalinkHttpDigest;

// ---- Link header parsing ----

#[test]
fn test_simple_link_header() {
    let links =
        MetalinkHttpParser::parse_link_header(r#"<http://mirror1>; rel="duplicate"; pri="1""#);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "http://mirror1");
    assert_eq!(links[0].rel, vec!["duplicate"]);
    assert_eq!(links[0].pri, Some(1));
}

#[test]
fn test_multiple_links_in_one_header() {
    let links = MetalinkHttpParser::parse_link_header(
        r#"<http://mirror1>; rel="duplicate"; pri="1", <http://mirror2>; rel="mirror"; pri="2""#,
    );
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].uri, "http://mirror1");
    assert_eq!(links[0].pri, Some(1));
    assert_eq!(links[1].uri, "http://mirror2");
    assert_eq!(links[1].pri, Some(2));
}

#[test]
fn test_link_with_all_parameters() {
    let links = MetalinkHttpParser::parse_link_header(
        r#"<http://example.com/file>; rel="duplicate"; pri="3"; type="application/octet-stream"; hreflang="en"; geo="US"; pref"#,
    );
    assert_eq!(links.len(), 1);
    let link = &links[0];
    assert_eq!(link.uri, "http://example.com/file");
    assert_eq!(link.rel, vec!["duplicate"]);
    assert_eq!(link.pri, Some(3));
    assert_eq!(link.type_.as_deref(), Some("application/octet-stream"));
    assert_eq!(link.lang.as_deref(), Some("en"));
    assert_eq!(link.geo.as_deref(), Some("us")); // lowercased
    assert!(link.pref);
}

#[test]
fn test_pref_sorts_first() {
    let links = MetalinkHttpParser::parse_link_header(
        r#"<http://a>; rel="duplicate"; pri="1", <http://b>; rel="duplicate"; pri="5"; pref"#,
    );
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].uri, "http://b"); // pref first
    assert!(links[0].pref);
    assert_eq!(links[1].uri, "http://a");
}

#[test]
fn test_priority_sorting() {
    let links = MetalinkHttpParser::parse_link_header(
        r#"<http://c>; rel="duplicate"; pri="3", <http://a>; rel="duplicate"; pri="1", <http://b>; rel="duplicate"; pri="2""#,
    );
    assert_eq!(links.len(), 3);
    assert_eq!(links[0].uri, "http://a");
    assert_eq!(links[1].uri, "http://b");
    assert_eq!(links[2].uri, "http://c");
}

#[test]
fn test_no_pri_is_lowest_priority() {
    let links = MetalinkHttpParser::parse_link_header(
        r#"<http://no-pri>; rel="duplicate", <http://with-pri>; rel="duplicate"; pri="1""#,
    );
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].uri, "http://with-pri");
    assert_eq!(links[1].uri, "http://no-pri");
    assert_eq!(links[1].pri, None);
}

#[test]
fn test_non_relevant_rel_filtered() {
    let links = MetalinkHttpParser::parse_link_header(
        r#"<http://next>; rel="next", <http://dup>; rel="duplicate""#,
    );
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "http://dup");
}

#[test]
fn test_mirror_rel_accepted() {
    let links = MetalinkHttpParser::parse_link_header(r#"<http://mirror>; rel="mirror""#);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].rel, vec!["mirror"]);
}

#[test]
fn test_malformed_missing_uri() {
    let links = MetalinkHttpParser::parse_link_header(r#"rel="duplicate"; pri="1""#);
    assert!(links.is_empty());
}

#[test]
fn test_malformed_missing_closing_bracket() {
    let links = MetalinkHttpParser::parse_link_header(r#"<http://incomplete; rel="duplicate""#);
    assert!(links.is_empty());
}

#[test]
fn test_malformed_empty_uri() {
    let links = MetalinkHttpParser::parse_link_header(r#"<   >; rel="duplicate""#);
    assert!(links.is_empty());
}

#[test]
fn test_empty_header() {
    let links = MetalinkHttpParser::parse_link_header("");
    assert!(links.is_empty());
}

#[test]
fn test_comma_inside_quoted_value() {
    let links = MetalinkHttpParser::parse_link_header(
        r#"<http://example.com>; rel="duplicate"; title="hello, world""#,
    );
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].uri, "http://example.com");
}

#[test]
fn test_unquoted_rel() {
    let links = MetalinkHttpParser::parse_link_header(r#"<http://example.com>; rel=duplicate"#);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].rel, vec!["duplicate"]);
}

#[test]
fn test_pri_out_of_range() {
    let links =
        MetalinkHttpParser::parse_link_header(r#"<http://example.com>; rel="duplicate"; pri="0""#);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].pri, None); // 0 is out of [1, 999999]
}

#[test]
fn test_rel_space_separated() {
    let links =
        MetalinkHttpParser::parse_link_header(r#"<http://example.com>; rel="duplicate mirror""#);
    assert_eq!(links[0].rel, vec!["duplicate", "mirror"]);
    assert!(links[0].is_relevant());
}

#[test]
fn test_geo_lowercased() {
    let links =
        MetalinkHttpParser::parse_link_header(r#"<http://example.com>; rel="duplicate"; geo="US""#);
    assert_eq!(links[0].geo.as_deref(), Some("us"));
}

// ---- Digest header parsing ----

#[test]
fn test_digest_simple() {
    let digests = MetalinkHttpParser::parse_digest_header("sha-256=abc123,md5=def456");
    assert_eq!(digests.len(), 2);
    assert_eq!(digests[0].algorithm, "sha-256");
    assert_eq!(digests[0].value, "abc123");
    assert_eq!(digests[1].algorithm, "md5");
    assert_eq!(digests[1].value, "def456");
}

#[test]
fn test_digest_quoted_value() {
    let digests = MetalinkHttpParser::parse_digest_header(r#"sha-256="base64value""#);
    assert_eq!(digests.len(), 1);
    assert_eq!(digests[0].algorithm, "sha-256");
    assert_eq!(digests[0].value, "base64value");
}

#[test]
fn test_digest_algorithm_lowercased() {
    let digests = MetalinkHttpParser::parse_digest_header("SHA-256=abc123");
    assert_eq!(digests[0].algorithm, "sha-256");
}

#[test]
fn test_digest_empty_value_skipped() {
    let digests = MetalinkHttpParser::parse_digest_header("sha-256=");
    assert!(digests.is_empty());
}

#[test]
fn test_digest_empty_algorithm_skipped() {
    let digests = MetalinkHttpParser::parse_digest_header("=abc123");
    assert!(digests.is_empty());
}

#[test]
fn test_digest_empty_header() {
    let digests = MetalinkHttpParser::parse_digest_header("");
    assert!(digests.is_empty());
}

// ---- Response parsing ----

#[test]
fn test_parse_response_with_headers() {
    use crate::http::header_processor::HttpHeaderProcessor;

    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\nLink: <http://mirror>; rel=\"duplicate\"; pri=\"1\"\r\nDigest: sha-256=abc123\r\n\r\n");
    let head = proc.get_result().unwrap();

    let result = MetalinkHttpParser::parse_response(&head, &[]);
    assert_eq!(result.links.len(), 1);
    assert_eq!(result.links[0].uri, "http://mirror");
    assert_eq!(result.digests.len(), 1);
    assert_eq!(result.digests[0].algorithm, "sha-256");
}

#[test]
fn test_parse_response_multiple_link_headers() {
    use crate::http::header_processor::HttpHeaderProcessor;

    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\nLink: <http://m1>; rel=\"duplicate\"; pri=\"1\"\r\nLink: <http://m2>; rel=\"duplicate\"; pri=\"2\"\r\n\r\n");
    let head = proc.get_result().unwrap();

    let result = MetalinkHttpParser::parse_response(&head, &[]);
    assert_eq!(result.links.len(), 2);
    assert_eq!(result.links[0].uri, "http://m1");
    assert_eq!(result.links[1].uri, "http://m2");
}

#[test]
fn test_parse_response_no_metalink_headers() {
    use crate::http::header_processor::HttpHeaderProcessor;

    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n");
    let head = proc.get_result().unwrap();

    let result = MetalinkHttpParser::parse_response(&head, &[]);
    assert!(result.links.is_empty());
    assert!(result.digests.is_empty());
}

// ---- metalink-location preference ----

#[test]
fn test_metalink_location_preference() {
    use crate::http::header_processor::HttpHeaderProcessor;

    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\nLink: <http://us-mirror>; rel=\"duplicate\"; pri=\"1\"; geo=\"us\"\r\nLink: <http://jp-mirror>; rel=\"duplicate\"; pri=\"2\"; geo=\"jp\"\r\n\r\n");
    let head = proc.get_result().unwrap();

    // Without location preference: us-mirror (pri=1) comes first
    let result = MetalinkHttpParser::parse_response(&head, &[]);
    assert_eq!(result.links[0].uri, "http://us-mirror");
    assert_eq!(result.links[1].uri, "http://jp-mirror");

    // With JP location preference: jp-mirror should be boosted
    let result = MetalinkHttpParser::parse_response(&head, &["JP".to_string()]);
    // jp-mirror should now come first (pri boosted from 2 to 2-999999)
    assert_eq!(
        result.links[0].uri, "http://jp-mirror",
        "JP mirror should be first after location preference"
    );
}

// ---- Digest deduplication ----

#[test]
fn test_digest_deduplication_consistent() {
    let digests = vec![
        MetalinkHttpDigest {
            algorithm: "sha-256".to_string(),
            value: "abc123".to_string(),
        },
        MetalinkHttpDigest {
            algorithm: "sha-256".to_string(),
            value: "abc123".to_string(),
        },
    ];
    let deduped = deduplicate_digests(digests);
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].algorithm, "sha-256");
}

#[test]
fn test_digest_deduplication_inconsistent() {
    let digests = vec![
        MetalinkHttpDigest {
            algorithm: "sha-256".to_string(),
            value: "abc123".to_string(),
        },
        MetalinkHttpDigest {
            algorithm: "sha-256".to_string(),
            value: "different".to_string(),
        },
    ];
    let deduped = deduplicate_digests(digests);
    assert!(
        deduped.is_empty(),
        "Inconsistent digests should be removed entirely"
    );
}

#[test]
fn test_digest_deduplication_multiple_algorithms() {
    let digests = vec![
        MetalinkHttpDigest {
            algorithm: "sha-256".to_string(),
            value: "abc123".to_string(),
        },
        MetalinkHttpDigest {
            algorithm: "md5".to_string(),
            value: "def456".to_string(),
        },
    ];
    let deduped = deduplicate_digests(digests);
    assert_eq!(deduped.len(), 2);
}

// ---- Multiple Digest headers ----

#[test]
fn test_parse_response_multiple_digest_headers() {
    use crate::http::header_processor::HttpHeaderProcessor;

    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\nDigest: sha-256=abc123\r\nDigest: md5=def456\r\n\r\n");
    let head = proc.get_result().unwrap();

    let result = MetalinkHttpParser::parse_response(&head, &[]);
    assert_eq!(result.digests.len(), 2);
    // Order from HashMap is not guaranteed, so check by content
    let algorithms: Vec<&str> = result
        .digests
        .iter()
        .map(|d| d.algorithm.as_str())
        .collect();
    assert!(algorithms.contains(&"sha-256"));
    assert!(algorithms.contains(&"md5"));
}

// ---- Internal helpers ----

#[test]
fn test_split_link_entries_respects_quotes() {
    let entries =
        split_link_entries(r#"<http://a>; title="hello, world", <http://b>; rel="duplicate""#);
    assert_eq!(entries.len(), 2);
}

#[test]
fn test_unquote_helper() {
    assert_eq!(unquote(r#""hello""#), "hello");
    assert_eq!(unquote(r#""he\"llo""#), "he\"llo");
    assert_eq!(unquote("naked"), "naked");
    assert_eq!(unquote(r#""""#), "");
}
