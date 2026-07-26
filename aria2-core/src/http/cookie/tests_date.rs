//! Tests for RFC 6265 Section 5.1.1 date parsing algorithm.

use super::Cookie;
use super::parsing::{format_http_date, parse_http_date};

#[test]
fn test_parse_http_date_imf_fixdate() {
    // Standard IMF-fixdate format: "Wed, 09 Jun 2021 10:18:14 GMT"
    let ts = parse_http_date("Wed, 09 Jun 2021 10:18:14 GMT").unwrap();
    assert!(ts > 0);
    // Verify round-trip: format back and check the date string contains expected parts
    let formatted = format_http_date(ts);
    assert!(
        formatted.contains("Jun"),
        "Formatted date should contain 'Jun': {}",
        formatted
    );
    assert!(
        formatted.contains("2021"),
        "Formatted date should contain '2021': {}",
        formatted
    );
}

#[test]
fn test_parse_http_date_rfc_850() {
    // RFC 850 format: "Sunday, 06-Nov-94 08:49:37 GMT"
    // Per RFC 6265 Section 5.1.1: tokens can appear in any order
    let ts = parse_http_date("06-Nov-94 08:49:37 GMT").unwrap();
    assert!(ts > 0);
    // Nov 6, 1994 08:49:37 GMT ~ 784629777
    assert!(
        ts > 784000000 && ts < 785000000,
        "Should parse RFC 850 date, got {}",
        ts
    );
}

#[test]
fn test_parse_http_date_asctime() {
    // ANSI C asctime format: "Sun Nov  6 08:49:37 1994"
    // The RFC 6265 algorithm handles tokens in any order, so this works too.
    let ts = parse_http_date("Sun Nov  6 08:49:37 1994").unwrap();
    assert!(ts > 0);
}

#[test]
fn test_parse_http_date_two_digit_year() {
    // Per RFC 6265 Section 5.1.1: 70-99 -> 1970-1999, 0-69 -> 2000-2069
    let ts_94 = parse_http_date("06 Nov 94 08:49:37 GMT").unwrap();
    let ts_1994 = parse_http_date("06 Nov 1994 08:49:37 GMT").unwrap();
    assert_eq!(ts_94, ts_1994, "2-digit year 94 should normalize to 1994");

    let ts_24 = parse_http_date("06 Nov 24 08:49:37 GMT").unwrap();
    let ts_2024 = parse_http_date("06 Nov 2024 08:49:37 GMT").unwrap();
    assert_eq!(ts_24, ts_2024, "2-digit year 24 should normalize to 2024");
}

#[test]
fn test_parse_http_date_invalid_rejected() {
    // Invalid dates must return None
    assert!(parse_http_date("").is_none());
    assert!(parse_http_date("not-a-date").is_none());
    assert!(
        parse_http_date("32 Jan 2021 00:00:00 GMT").is_none(),
        "Day 32 is invalid"
    );
    assert!(
        parse_http_date("29 Feb 2021 00:00:00 GMT").is_none(),
        "2021 is not a leap year"
    );
    assert!(
        parse_http_date("29 Feb 2020 00:00:00 GMT").is_some(),
        "2020 IS a leap year"
    );
    assert!(
        parse_http_date("31 Apr 2021 00:00:00 GMT").is_none(),
        "April has 30 days"
    );
}

#[test]
fn test_parse_http_date_year_before_1601() {
    // Per RFC 6265 Section 5.1.1: year must be >= 1601
    assert!(parse_http_date("01 Jan 1600 00:00:00 GMT").is_none());
    assert!(parse_http_date("01 Jan 1601 00:00:00 GMT").is_some());
}

#[test]
fn test_parse_http_date_tokens_in_any_order() {
    // The RFC 6265 Section 5.1.1 algorithm identifies tokens by their
    // pattern (time=HH:MM:SS, month=name, day=1-2 digits, year=1-4 digits)
    // regardless of position. This test verifies non-standard ordering.
    let ts1 = parse_http_date("08:49:37 06 Nov 1994").unwrap();
    let ts2 = parse_http_date("06 Nov 1994 08:49:37").unwrap();
    assert_eq!(
        ts1, ts2,
        "Tokens in different order should produce same result"
    );
}

#[test]
fn test_parse_http_date_expires_in_set_cookie() {
    // Verify that the full date parser works correctly when used via
    // Cookie::from_set_cookie_header with Expires attribute
    let hdr = "session=abc; Expires=Fri, 01 Jan 2038 00:00:00 GMT; Path=/; domain=localhost";
    let c = Cookie::from_set_cookie_header(hdr, "localhost", "/").unwrap();
    assert!(c.persistent, "Cookie with Expires should be persistent");
    assert!(c.expiry_time > 0, "Expiry time should be set");
}

#[test]
fn test_format_http_date_roundtrip() {
    // Format a known timestamp and parse it back
    let original_ts: i64 = 1735689600; // 2025-01-01 00:00:00 UTC
    let formatted = format_http_date(original_ts);
    let parsed = parse_http_date(&formatted).unwrap();
    // Allow up to 1 second tolerance due to potential rounding
    assert!(
        (parsed - original_ts).abs() <= 1,
        "Round-trip should be within 1s: original={}, parsed={}",
        original_ts,
        parsed
    );
}
