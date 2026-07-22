//! Tests for file entry management.

#![allow(unused_imports)]

use std::collections::VecDeque;
use std::sync::Arc;

use super::entry::FileEntry;
use super::helpers::{
    count_requested_file_entry, extract_host, get_first_requested_file_entry,
    is_uri_supplied_for_requested_file_entry, is_valid_uri,
};
use crate::download::request::Request;

// ── Construction ─────────────────────────────────────────────────────

#[test]
fn test_default_construction() {
    let entry = FileEntry::default();
    assert_eq!(entry.length(), 0);
    assert_eq!(entry.offset(), 0);
    assert!(!entry.is_requested());
    assert!(!entry.is_unique_protocol());
    assert!(entry.remaining_uris().is_empty());
    assert!(entry.spent_uris().is_empty());
    assert!(entry.path().is_empty());
    assert!(entry.content_type().is_empty());
    assert!(entry.original_name().is_empty());
    assert!(entry.suffix_path().is_empty());
    assert_eq!(entry.max_connection_per_server(), 1);
}

#[test]
fn test_parameterized_construction() {
    let entry = FileEntry::new(
        "/downloads/file.zip".to_string(),
        1024,
        2048,
        vec!["http://example.com/file.zip".to_string()],
    );
    assert_eq!(entry.path(), "/downloads/file.zip");
    assert_eq!(entry.length(), 1024);
    assert_eq!(entry.offset(), 2048);
    assert!(entry.is_requested());
    assert_eq!(entry.remaining_uris().len(), 1);
}

// ── Path management ──────────────────────────────────────────────────

#[test]
fn test_path_accessors() {
    let mut entry = FileEntry::default();
    entry.set_path("/downloads/file.zip".to_string());
    assert_eq!(entry.path(), "/downloads/file.zip");
    assert_eq!(entry.basename(), "file.zip");
    assert_eq!(entry.dirname(), "/downloads");
}

#[test]
fn test_basename_empty_path() {
    let entry = FileEntry::default();
    assert!(entry.basename().is_empty());
}

#[test]
fn test_dirname_empty_path() {
    let entry = FileEntry::default();
    assert!(entry.dirname().is_empty());
}

#[test]
fn test_original_name() {
    let mut entry = FileEntry::default();
    assert!(entry.original_name().is_empty());
    entry.set_original_name("original.zip".to_string());
    assert_eq!(entry.original_name(), "original.zip");
}

#[test]
fn test_suffix_path() {
    let mut entry = FileEntry::default();
    assert!(entry.suffix_path().is_empty());
    entry.set_suffix_path("file.zip".to_string());
    assert_eq!(entry.suffix_path(), "file.zip");
}

#[test]
fn test_content_type() {
    let mut entry = FileEntry::default();
    assert!(entry.content_type().is_empty());
    entry.set_content_type("application/zip".to_string());
    assert_eq!(entry.content_type(), "application/zip");
}

// ── Length / Offset ──────────────────────────────────────────────────

#[test]
fn test_length_offset() {
    let mut entry = FileEntry::default();
    entry.set_length(1024);
    entry.set_offset(2048);
    assert_eq!(entry.length(), 1024);
    assert_eq!(entry.offset(), 2048);
    assert_eq!(entry.last_offset(), 3072);
}

#[test]
fn test_last_offset_saturating() {
    let mut entry = FileEntry::default();
    entry.set_length(u64::MAX);
    entry.set_offset(1);
    assert_eq!(entry.last_offset(), u64::MAX); // saturating_add
}

#[test]
fn test_gtoloff() {
    let mut entry = FileEntry::default();
    entry.set_offset(1000);
    assert_eq!(entry.gtoloff(1000), 0);
    assert_eq!(entry.gtoloff(1500), 500);
}

#[test]
#[should_panic]
fn test_gtoloff_panics_on_invalid_offset() {
    let mut entry = FileEntry::default();
    entry.set_offset(1000);
    entry.gtoloff(500); // should panic in debug
}

// ── Requested / UniqueProtocol ───────────────────────────────────────

#[test]
fn test_requested_flag() {
    let mut entry = FileEntry::default();
    assert!(!entry.is_requested());
    entry.set_requested(true);
    assert!(entry.is_requested());
}

#[test]
fn test_unique_protocol_flag() {
    let mut entry = FileEntry::default();
    assert!(!entry.is_unique_protocol());
    entry.set_unique_protocol(true);
    assert!(entry.is_unique_protocol());
}

// ── URI management ───────────────────────────────────────────────────

#[test]
fn test_add_uri_valid() {
    let mut entry = FileEntry::default();
    assert!(entry.add_uri("http://example.com/file.zip"));
    assert_eq!(entry.remaining_uris().len(), 1);
    assert_eq!(entry.remaining_uris()[0], "http://example.com/file.zip");
}

#[test]
fn test_add_uri_invalid() {
    let mut entry = FileEntry::default();
    assert!(!entry.add_uri("not a url"));
    assert!(entry.remaining_uris().is_empty());
}

#[test]
fn test_add_uris() {
    let mut entry = FileEntry::default();
    let count = entry.add_uris(&[
        "http://a.com/file".to_string(),
        "http://b.com/file".to_string(),
        "invalid".to_string(),
    ]);
    assert_eq!(count, 2);
    assert_eq!(entry.remaining_uris().len(), 2);
}

#[test]
fn test_set_uris() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://old.com/file");
    let count = entry.set_uris(&[
        "http://new1.com/file".to_string(),
        "http://new2.com/file".to_string(),
    ]);
    assert_eq!(count, 2);
    assert_eq!(entry.remaining_uris().len(), 2);
}

#[test]
fn test_insert_uri() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://a.com/file");
    entry.add_uri("http://c.com/file");
    assert!(entry.insert_uri("http://b.com/file", 1));
    assert_eq!(entry.remaining_uris().len(), 3);
    assert_eq!(entry.remaining_uris()[1], "http://b.com/file");
}

#[test]
fn test_insert_uri_at_end() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://a.com/file");
    assert!(entry.insert_uri("http://b.com/file", 100)); // pos > len
    assert_eq!(entry.remaining_uris().len(), 2);
}

#[test]
fn test_uris_concatenated() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://remaining.com/file");
    entry.spent_uris.push_back("http://spent.com/file".to_string());
    let all = entry.uris();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0], "http://spent.com/file");
    assert_eq!(all[1], "http://remaining.com/file");
}

#[test]
fn test_remove_uri_from_remaining() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://a.com/file");
    entry.add_uri("http://b.com/file");
    assert!(entry.remove_uri("http://a.com/file"));
    assert_eq!(entry.remaining_uris().len(), 1);
    assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
}

#[test]
fn test_remove_uri_not_found() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://a.com/file");
    assert!(!entry.remove_uri("http://nonexistent.com/file"));
}

#[test]
fn test_remove_uri_from_spent() {
    let mut entry = FileEntry::default();
    entry.spent_uris
        .push_back("http://spent.com/file".to_string());
    assert!(entry.remove_uri("http://spent.com/file"));
    assert!(entry.spent_uris().is_empty());
}

#[test]
fn test_remove_uri_whose_hostname_is() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://a.com/file1");
    entry.add_uri("http://b.com/file2");
    entry.add_uri("http://a.com/file3");
    entry.remove_uri_whose_hostname_is("a.com");
    assert_eq!(entry.remaining_uris().len(), 1);
    assert_eq!(entry.remaining_uris()[0], "http://b.com/file2");
}

#[test]
fn test_remove_identical_uri() {
    let mut entry = FileEntry::default();
    entry.add_uri("http://a.com/file");
    entry.add_uri("http://a.com/file"); // duplicate
    entry.add_uri("http://b.com/file");
    entry.remove_identical_uri("http://a.com/file");
    assert_eq!(entry.remaining_uris().len(), 1);
    assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
}

#[test]
fn test_empty_request_uri() {
    let mut entry = FileEntry::default();
    assert!(entry.empty_request_uri());
    entry.add_uri("http://a.com/file");
    assert!(!entry.empty_request_uri());
}

// ── URI results ──────────────────────────────────────────────────────

#[test]
fn test_add_uri_result() {
    let mut entry = FileEntry::default();
    entry.add_uri_result("http://a.com/file".to_string(), 1);
    entry.add_uri_result("http://b.com/file".to_string(), 2);
    assert_eq!(entry.uri_results().len(), 2);
}

#[test]
fn test_extract_uri_result() {
    let mut entry = FileEntry::default();
    entry.add_uri_result("http://a.com/file".to_string(), 1);
    entry.add_uri_result("http://b.com/file".to_string(), 2);
    entry.add_uri_result("http://c.com/file".to_string(), 1);

    let mut extracted = VecDeque::new();
    entry.extract_uri_result(&mut extracted, 1);
    assert_eq!(extracted.len(), 2);
    assert_eq!(entry.uri_results().len(), 1);
    assert_eq!(entry.uri_results()[0].result_code, 2);
}

// ── Request pool / in-flight ─────────────────────────────────────────

#[test]
fn test_pool_request() {
    let mut entry = FileEntry::default();
    let req = Request::new("http://example.com/file").unwrap();
    let req = Arc::new(req);
    // Add to in-flight first.
    entry.in_flight_requests.push(Arc::clone(&req));
    // Pool it.
    entry.pool_request(&req);
    assert_eq!(entry.count_in_flight_request(), 0);
    assert_eq!(entry.count_pooled_request(), 1);
}

#[test]
fn test_pool_request_removal_requested() {
    let mut entry = FileEntry::default();
    let mut req = Request::new("http://example.com/file").unwrap();
    req.request_removal();
    let req = Arc::new(req);
    entry.in_flight_requests.push(Arc::clone(&req));
    entry.pool_request(&req);
    // Should be discarded, not pooled.
    assert_eq!(entry.count_in_flight_request(), 0);
    assert_eq!(entry.count_pooled_request(), 0);
}

#[test]
fn test_remove_request() {
    let mut entry = FileEntry::default();
    let req = Request::new("http://example.com/file").unwrap();
    let req = Arc::new(req);
    entry.in_flight_requests.push(Arc::clone(&req));
    assert!(entry.remove_request(&req));
    assert_eq!(entry.count_in_flight_request(), 0);
}

#[test]
fn test_remove_request_not_found() {
    let mut entry = FileEntry::default();
    let req = Request::new("http://example.com/file").unwrap();
    let req = Arc::new(req);
    assert!(!entry.remove_request(&req));
}

// ── Connection control ───────────────────────────────────────────────

#[test]
fn test_max_connection_per_server() {
    let mut entry = FileEntry::default();
    assert_eq!(entry.max_connection_per_server(), 1);
    entry.set_max_connection_per_server(4);
    assert_eq!(entry.max_connection_per_server(), 4);
}

#[test]
fn test_max_connection_per_server_minimum() {
    let mut entry = FileEntry::default();
    entry.set_max_connection_per_server(0); // should clamp to 1
    assert_eq!(entry.max_connection_per_server(), 1);
}

// ── Runtime resource management ──────────────────────────────────────

#[test]
fn test_release_runtime_resource() {
    let mut entry = FileEntry::default();
    let req = Arc::new(Request::new("http://example.com/file").unwrap());
    entry.in_flight_requests.push(Arc::clone(&req));
    entry.request_pool.push(req);
    entry.release_runtime_resource();
    assert_eq!(entry.count_in_flight_request(), 0);
    assert_eq!(entry.count_pooled_request(), 0);
}

// ── File existence ───────────────────────────────────────────────────

#[test]
fn test_exists_empty_path() {
    let entry = FileEntry::default();
    assert!(!entry.exists());
}

#[test]
fn test_exists_nonexistent_file() {
    let mut entry = FileEntry::default();
    entry.set_path("/nonexistent/path/file.zip".to_string());
    assert!(!entry.exists());
}

// ── Comparison ───────────────────────────────────────────────────────

#[test]
fn test_comparison_by_offset() {
    let mut e1 = FileEntry::default();
    let mut e2 = FileEntry::default();
    e1.set_offset(100);
    e2.set_offset(200);
    assert!(e1 < e2);
    assert!(e2 > e1);
}

#[test]
fn test_eq_same_offset() {
    let mut e1 = FileEntry::default();
    let mut e2 = FileEntry::default();
    e1.set_offset(100);
    e2.set_offset(100);
    assert_eq!(e1, e2);
}

// ── URI reuse ────────────────────────────────────────────────────────

#[test]
fn test_reuse_uri_basic() {
    let mut entry = FileEntry::default();
    // Simulate: spent URIs without errors should be reusable.
    entry.spent_uris
        .push_back("http://a.com/file".to_string());
    entry.spent_uris
        .push_back("http://b.com/file".to_string());
    // One URI had an error.
    entry.add_uri_result("http://a.com/file".to_string(), 2);

    entry.reuse_uri(&[]);
    // Only b.com should be reusable.
    assert_eq!(entry.remaining_uris().len(), 1);
    assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
}

#[test]
fn test_reuse_uri_with_ignore() {
    let mut entry = FileEntry::default();
    entry.spent_uris
        .push_back("http://a.com/file".to_string());
    entry.spent_uris
        .push_back("http://b.com/file".to_string());

    entry.reuse_uri(&["a.com".to_string()]);
    // a.com should be ignored.
    assert_eq!(entry.remaining_uris().len(), 1);
    assert_eq!(entry.remaining_uris()[0], "http://b.com/file");
}

// ── putBackRequest ───────────────────────────────────────────────────

#[test]
fn test_put_back_request() {
    let mut entry = FileEntry::default();
    let req1 = Arc::new(Request::new("http://a.com/file").unwrap());
    let req2 = Arc::new(Request::new("http://b.com/file").unwrap());
    entry.request_pool.push(Arc::clone(&req1));
    entry.in_flight_requests.push(Arc::clone(&req2));

    entry.put_back_request();
    // URIs should be at front of remaining_uris.
    assert_eq!(entry.remaining_uris().len(), 2);
}

// ── Free functions ───────────────────────────────────────────────────

#[test]
fn test_get_first_requested_file_entry() {
    let e1 = Arc::new(FileEntry::default()); // not requested
    let mut e2 = FileEntry::default();
    e2.set_requested(true);
    let e2 = Arc::new(e2);

    let entries = vec![e1, e2];
    let result = get_first_requested_file_entry(&entries);
    assert!(result.is_some());
    assert!(result.unwrap().is_requested());
}

#[test]
fn test_get_first_requested_file_entry_none() {
    let entries: Vec<Arc<FileEntry>> = vec![Arc::new(FileEntry::default())];
    assert!(get_first_requested_file_entry(&entries).is_none());
}

#[test]
fn test_count_requested_file_entry() {
    let mut e1 = FileEntry::default();
    e1.set_requested(true);
    let e2 = FileEntry::default();
    let entries = vec![Arc::new(e1), Arc::new(e2)];
    assert_eq!(count_requested_file_entry(&entries), 1);
}

#[test]
fn test_is_uri_supplied_for_requested_file_entry() {
    let mut e1 = FileEntry::default();
    e1.set_requested(true);
    e1.add_uri("http://example.com/file");
    let entries = vec![Arc::new(e1)];
    assert!(is_uri_supplied_for_requested_file_entry(&entries));
}

#[test]
fn test_is_uri_supplied_no_uris() {
    let mut e1 = FileEntry::default();
    e1.set_requested(true);
    // No URIs.
    let entries = vec![Arc::new(e1)];
    assert!(!is_uri_supplied_for_requested_file_entry(&entries));
}

// ── URI validation ───────────────────────────────────────────────────

#[test]
fn test_is_valid_uri() {
    assert!(is_valid_uri("http://example.com/file.zip"));
    assert!(is_valid_uri("https://example.com:8443/path"));
    assert!(is_valid_uri("ftp://ftp.example.com/pub/file"));
    assert!(!is_valid_uri("not a url"));
    assert!(!is_valid_uri(""));
}

// ── Extract host ─────────────────────────────────────────────────────

#[test]
fn test_extract_host() {
    assert_eq!(
        extract_host("http://example.com/path"),
        Some("example.com".to_string())
    );
    assert_eq!(
        extract_host("https://cdn.example.com:8443/file"),
        Some("cdn.example.com:8443".to_string())
    );
    assert_eq!(extract_host("invalid"), None);
}
