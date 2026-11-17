use super::super::{MAX_RETRY_AFTER, parse_retry_after};
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn parses_retry_after_seconds_and_rejects_invalid_values() {
    let mut headers = HeaderMap::new();
    assert_eq!(parse_retry_after(&headers), None);

    headers.insert(RETRY_AFTER, HeaderValue::from_static("42"));
    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(42)));

    headers.insert(RETRY_AFTER, HeaderValue::from_static("not a date"));
    assert_eq!(parse_retry_after(&headers), None);
}

#[test]
fn parses_http_dates_and_caps_long_waits() {
    let mut headers = HeaderMap::new();
    headers.insert(
        RETRY_AFTER,
        HeaderValue::from_static("Wed, 09 Jun 2021 10:18:14 GMT"),
    );
    assert_eq!(parse_retry_after(&headers), Some(Duration::ZERO));

    headers.insert(RETRY_AFTER, HeaderValue::from_static("3600"));
    assert_eq!(parse_retry_after(&headers), Some(MAX_RETRY_AFTER));

    let target = SystemTime::now() + Duration::from_secs(3600);
    let epoch = target.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let date = aria2_protocol::http::cookie::parsing::format_http_date(epoch);
    headers.insert(RETRY_AFTER, HeaderValue::from_str(&date).unwrap());
    assert_eq!(parse_retry_after(&headers), Some(MAX_RETRY_AFTER));
}
