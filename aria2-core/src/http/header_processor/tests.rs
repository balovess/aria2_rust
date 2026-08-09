//! Tests for the streaming HTTP header processor.

use super::processor::HttpHeaderProcessor;
use super::types::{HttpHeaderParseState, MAX_HEADER_SIZE};

#[test]
fn test_simple_headers_complete_in_one_feed() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 42\r\n\r\n";
    let state = proc.feed(data);
    assert!(state.is_complete());

    let head = proc.get_result().unwrap();
    assert_eq!(head.http_version, "HTTP/1.1");
    assert_eq!(head.status_code, 200);
    assert_eq!(head.reason_phrase, "OK");
    assert_eq!(head.header("content-type"), Some("text/html"));
    assert_eq!(head.content_length(), Some(42));
}

#[test]
fn test_incremental_feeding() {
    let mut proc = HttpHeaderProcessor::new();

    // First chunk: status line only
    let state = proc.feed(b"HTTP/1.1 302 Found\r\n");
    assert!(!state.is_complete());
    assert_eq!(proc.last_bytes_processed(), 20);

    // Second chunk: one header
    let state = proc.feed(b"Location: /new\r\n");
    assert!(!state.is_complete());
    assert_eq!(proc.last_bytes_processed(), 16);

    // Third chunk: terminator
    let state = proc.feed(b"\r\n");
    assert!(state.is_complete());
    assert_eq!(proc.last_bytes_processed(), 2);

    let head = proc.get_result().unwrap();
    assert_eq!(head.status_code, 302);
    assert_eq!(head.reason_phrase, "Found");
    assert_eq!(head.header("location"), Some("/new"));
}

#[test]
fn test_body_bytes_after_headers() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\n\r\n<body data here>";
    let state = proc.feed(data);
    assert!(state.is_complete());

    // Only 19 bytes are header bytes (HTTP/1.1 200 OK\r\n\r\n)
    assert_eq!(proc.last_bytes_processed(), 19);

    let head = proc.get_result().unwrap();
    assert_eq!(head.status_code, 200);
}

#[test]
fn test_body_bytes_across_feeds() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n");
    // This feed completes headers AND includes body bytes
    let state = proc.feed(b"\r\nhello");
    assert!(state.is_complete());

    // \r\n (terminator) = 2 bytes from this feed are header bytes
    assert_eq!(proc.last_bytes_processed(), 2);

    // "hello" (5 bytes) are body bytes, not in the header result
    let head = proc.get_result().unwrap();
    assert_eq!(head.header("content-length"), Some("5"));
}

#[test]
fn test_obs_fold_multiline_header() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nX-Custom: hello\r\n world\r\n\r\n";
    let state = proc.feed(data);
    assert!(state.is_complete());

    let head = proc.get_result().unwrap();
    // obs-fold: " world" appended to "hello" with space separator
    assert_eq!(head.header("x-custom"), Some("hello world"));
}

#[test]
fn test_obs_fold_tab() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nX-Folded: line1\r\n\tline2\r\n\r\n";
    let state = proc.feed(data);
    assert!(state.is_complete());

    let head = proc.get_result().unwrap();
    assert_eq!(head.header("x-folded"), Some("line1 line2"));
}

#[test]
fn test_obs_fold_without_previous_header_is_error() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\n continuation-without-name\r\n\r\n";
    proc.feed(data);

    // Error is detected during parse, not during feed
    let result = proc.get_result();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("LWS"));
}

#[test]
fn test_multiple_same_name_headers() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n";
    proc.feed(data);

    let head = proc.get_result().unwrap();
    let cookies = head.header_all("set-cookie");
    assert_eq!(cookies.len(), 2);
    assert_eq!(cookies[0], "a=1");
    assert_eq!(cookies[1], "b=2");
}

#[test]
fn test_case_insensitive_lookup() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n";
    proc.feed(data);

    let head = proc.get_result().unwrap();
    assert_eq!(head.header("content-type"), Some("text/html"));
    assert_eq!(head.header("CONTENT-TYPE"), Some("text/html"));
    assert_eq!(head.header("Content-Type"), Some("text/html"));
}

#[test]
fn test_transfer_encoding_overrides_content_length() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 999\r\nContent-Range: bytes 0-499/1000\r\n\r\n";
    proc.feed(data);

    let head = proc.get_result().unwrap();
    assert!(head.has_transfer_encoding());
    // Content-Length and Content-Range must be removed per RFC 7230
    assert_eq!(head.header("content-length"), None);
    assert_eq!(head.header("content-range"), None);
}

#[test]
fn test_malformed_status_line_missing_version() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"GARBAGE 200 OK\r\n\r\n";
    proc.feed(data);

    let result = proc.get_result();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("HTTP-version"));
}

#[test]
fn test_malformed_status_line_missing_code() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1\r\n\r\n";
    proc.feed(data);

    let result = proc.get_result();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("status-code"));
}

#[test]
fn test_malformed_status_line_invalid_code() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 abc OK\r\n\r\n";
    proc.feed(data);

    let result = proc.get_result();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("status-code"));
}

#[test]
fn test_malformed_status_code_below_100() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 099 OK\r\n\r\n";
    proc.feed(data);

    let result = proc.get_result();
    assert!(result.is_err());
}

#[test]
fn test_missing_end_of_headers() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n";
    let state = proc.feed(data);
    assert!(!state.is_complete());
}

#[test]
fn test_header_name_starts_with_colon() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\n: bad-name\r\n\r\n";
    proc.feed(data);

    let result = proc.get_result();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains(':'));
}

#[test]
fn test_header_missing_colon() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nNoColonHere\r\n\r\n";
    proc.feed(data);

    let result = proc.get_result();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains(':'));
}

#[test]
fn test_reason_phrase_with_spaces() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";
    proc.feed(data);

    let head = proc.get_result().unwrap();
    assert_eq!(head.status_code, 500);
    assert_eq!(head.reason_phrase, "Internal Server Error");
}

#[test]
fn test_no_reason_phrase() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200\r\n\r\n";
    proc.feed(data);

    let head = proc.get_result().unwrap();
    assert_eq!(head.status_code, 200);
    assert_eq!(head.reason_phrase, "");
}

#[test]
fn test_oversized_header_block() {
    let mut proc = HttpHeaderProcessor::new();
    // Feed data until buffer exceeds MAX_HEADER_SIZE
    let big_chunk = vec![b'X'; MAX_HEADER_SIZE + 1];
    let state = proc.feed(&big_chunk);
    assert!(state.is_error());
}

#[test]
fn test_clear_resets_state() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\n\r\n");
    assert!(proc.get_result().is_ok());

    proc.clear();
    assert_eq!(proc.state, HttpHeaderParseState::ParsingStatusLine);
    assert_eq!(proc.last_bytes_processed(), 0);

    // Can process a new response
    proc.feed(b"HTTP/1.1 404 Not Found\r\n\r\n");
    let head = proc.get_result().unwrap();
    assert_eq!(head.status_code, 404);
}

#[test]
fn test_feed_after_complete_is_noop() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\n\r\n");
    assert!(proc.state.is_complete());

    // Feed more data after completion — should not change state
    let state = proc.feed(b"extra body data");
    assert!(state.is_complete());
    assert_eq!(proc.last_bytes_processed(), 0);
}

#[test]
fn test_get_header_string() {
    let mut proc = HttpHeaderProcessor::new();
    let data = b"HTTP/1.1 200 OK\r\nServer: test\r\n\r\n";
    proc.feed(data);

    let header_str = proc.get_header_string();
    assert!(header_str.starts_with("HTTP/1.1 200 OK"));
    assert!(header_str.contains("Server: test"));
    assert!(header_str.ends_with("\r\n\r\n"));
}

#[test]
fn test_split_terminator_across_feeds() {
    let mut proc = HttpHeaderProcessor::new();
    // Feed ends with \r (first half of \r\n in the terminator)
    proc.feed(b"HTTP/1.1 200 OK\r\n\r");
    assert!(!proc.state.is_complete());
    // Complete the \r\n\r\n terminator
    let state = proc.feed(b"\n");
    assert!(state.is_complete());
}

#[test]
fn test_http1_0_response() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");

    let head = proc.get_result().unwrap();
    assert_eq!(head.http_version, "HTTP/1.0");
    assert_eq!(head.status_code, 200);
}

#[test]
fn test_iter_headers() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\n\r\n");

    let head = proc.get_result().unwrap();
    let pairs: Vec<_> = head.iter_headers().collect();
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0], ("a", "1"));
    assert_eq!(pairs[1], ("b", "2"));
}
