use super::{HttpHeaderParseState, HttpHeaderProcessor, find_header_end, parse_response_head};

#[test]
fn complete_response_parser_handles_header_block_and_reports_delimiter() {
    let raw = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\r\n";
    let end = find_header_end(raw).unwrap();
    assert_eq!(end + 4, raw.len());

    let head = parse_response_head(raw).unwrap();
    assert_eq!(head.status_code, 206);
    assert_eq!(head.content_length(), Some(4));
    let chunked =
        parse_response_head(b"HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
    assert_eq!(chunked.header("transfer-encoding"), Some("chunked"));
}

#[test]
fn parses_incremental_http_response_headers_and_stops_at_body() {
    let mut parser = HttpHeaderProcessor::new();
    assert!(
        !parser
            .feed(b"HTTP/1.1 206 Partial Content\r\n")
            .is_complete()
    );
    let state = parser.feed(b"Content-Range: bytes 0-3/4\r\n\r\nDATA");

    assert_eq!(state, &HttpHeaderParseState::Complete);
    assert_eq!(parser.last_bytes_processed(), 30);
    let response = parser.get_result().unwrap();
    assert_eq!(response.status_code, 206);
    assert_eq!(response.header("content-range"), Some("bytes 0-3/4"));
}

#[test]
fn rejects_invalid_header_block_and_resets_cleanly() {
    let mut parser = HttpHeaderProcessor::new();
    parser.feed(b"not a status line\r\n\r\n");
    assert!(parser.get_result().is_err());

    parser.clear();
    assert_eq!(
        parser.feed(b"HTTP/1.1 200 OK\r\n\r\n"),
        &HttpHeaderParseState::Complete
    );
    assert_eq!(parser.get_result().unwrap().status_code, 200);
}

#[test]
fn discards_a_large_body_without_buffering_it_as_header_data() {
    let mut parser = HttpHeaderProcessor::new();
    let mut input = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    input.resize(2 * 1024 * 1024, b'x');

    assert!(parser.feed(&input).is_complete());
    assert_eq!(parser.last_bytes_processed(), 19);
    assert_eq!(parser.get_header_string().len(), 19);
}
