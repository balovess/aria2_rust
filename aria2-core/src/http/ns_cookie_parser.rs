use std::fs;
use std::path::Path;

use crate::error::{Aria2Error, Result};
use crate::http::cookie::Cookie;

pub struct NsCookieParser;

impl NsCookieParser {
    pub fn parse_file(path: &Path) -> Result<Vec<Cookie>> {
        let data = fs::read_to_string(path).map_err(|e| Aria2Error::Io(e.to_string()))?;
        Self::parse_str(&data)
    }

    pub fn parse_str(data: &str) -> Result<Vec<Cookie>> {
        let mut cookies = Vec::new();
        for line in data.lines() {
            if let Some(c) = Self::parse_line(line) {
                cookies.push(c);
            }
        }
        Ok(cookies)
    }

    pub fn parse_line(line: &str) -> Option<Cookie> {
        Cookie::parse_netscape_line(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns_line(
        domain: &str,
        sub: &str,
        path: &str,
        secure: &str,
        expiry: &str,
        name: &str,
        value: &str,
    ) -> String {
        let t = "\t";
        [
            domain, t, sub, t, path, t, secure, t, expiry, t, name, t, value,
        ]
        .concat()
    }

    #[test]
    fn test_parse_standard_line() {
        let line = ns_line(
            ".example.com",
            "TRUE",
            "/",
            "FALSE",
            "0",
            "session_id",
            "abc123",
        );
        let c = NsCookieParser::parse_line(&line).unwrap();
        assert_eq!(c.domain, "example.com");
        assert_eq!(c.name, "session_id");
        assert_eq!(c.value, "abc123");
        assert!(!c.secure);
        // Per C++ NsCookieParser: expiry=0 means session cookie (persistent=false)
        assert!(!c.persistent, "expiry=0 should mean session cookie");
        assert!(!c.host_only, "include_subdomains=TRUE -> host_only=false");
    }

    #[test]
    fn test_parse_secure_line() {
        let line = ns_line(
            ".secure.com",
            "TRUE",
            "/",
            "TRUE",
            "0",
            "token",
            "secret_val",
        );
        let c = NsCookieParser::parse_line(&line).unwrap();
        assert!(c.secure);
        assert_eq!(c.value, "secret_val");
        // Per C++ NsCookieParser: expiry=0 means session cookie
        assert!(!c.persistent, "expiry=0 should mean session cookie");
    }

    #[test]
    fn test_skip_comment_lines() {
        let data = "# Netscape Cookie File\n# Generated\n";
        let cookies = NsCookieParser::parse_str(data).unwrap();
        assert_eq!(cookies.len(), 0);
    }

    #[test]
    fn test_skip_empty_lines() {
        let data = "\n\n";
        let cookies = NsCookieParser::parse_str(data).unwrap();
        assert_eq!(cookies.len(), 0);
    }

    #[test]
    fn test_insufficient_fields_returns_none() {
        assert!(NsCookieParser::parse_line("a\tb\tc").is_none());
        assert!(NsCookieParser::parse_line("").is_none());
    }

    #[test]
    fn test_parse_multiple_lines() {
        let l1 = ns_line(".a.com", "TRUE", "/", "FALSE", "1", "k1", "v1");
        let l2 = ns_line(".b.com", "TRUE", "/", "TRUE", "2", "k2", "v2");
        let l3 = ns_line(".c.com", "TRUE", "/", "FALSE", "3", "k3", "v3");
        let data = format!("{}\n{}\n{}", l1, l2, l3);
        let cookies = NsCookieParser::parse_str(&data).unwrap();
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[0].domain, "a.com");
        assert_eq!(cookies[1].domain, "b.com");
        assert_eq!(cookies[2].domain, "c.com");
    }

    #[test]
    fn test_value_with_internal_tab() {
        // Value may contain internal tabs; everything after the 6th tab is the value
        let line = ns_line(".x.com", "TRUE", "/", "FALSE", "0", "k", "val\tue");
        let c = NsCookieParser::parse_line(&line).unwrap();
        // The ns_line helper inserts tabs between all fields, so "val\tue" splits
        // into extra fields. Our parser joins them back.
        assert!(c.value.starts_with("val"));
        assert!(c.value.contains("ue"));
    }

    #[test]
    fn test_parse_subsecond_expiry() {
        // Chrome extension format uses subsecond resolution for expiry time.
        // Per C++ NsCookieParser: parse as double, truncate to integer.
        let line = ns_line(
            ".something.com",
            "TRUE",
            "/",
            "FALSE",
            "1463304912.5",
            "TAX",
            "1000",
        );
        let c = NsCookieParser::parse_line(&line).unwrap();
        assert_eq!(c.name, "TAX");
        assert_eq!(c.value, "1000");
        assert_eq!(
            c.expiry_time, 1463304912,
            "Fractional part must be truncated"
        );
        assert!(c.persistent);
    }

    #[test]
    fn test_parse_six_fields_no_value() {
        // Per C++ NsCookieParser: value field is optional.
        // A line with only 6 tab-separated fields (no value) is valid.
        let t = "\t";
        let line = [
            ".example.org",
            t,
            "TRUE",
            t,
            "/",
            t,
            "FALSE",
            t,
            "2147483647.123",
            t,
            "novalue",
        ]
        .concat();
        let c = NsCookieParser::parse_line(&line).unwrap();
        assert_eq!(c.name, "novalue");
        assert_eq!(
            c.value, "",
            "Missing value field should default to empty string"
        );
        assert!(c.persistent);
        assert_eq!(c.expiry_time, 2147483647);
        assert!(!c.host_only, "include_subdomains=TRUE -> host_only=false");
    }

    #[test]
    fn test_parse_numeric_host_forces_host_only() {
        let line = ns_line(
            "192.168.0.1",
            "TRUE",
            "/cgi-bin",
            "FALSE",
            "0",
            "passwd",
            "secret",
        );
        let c = NsCookieParser::parse_line(&line).unwrap();
        assert!(c.host_only, "Numeric host must force host-only mode");
        assert_eq!(c.domain, "192.168.0.1");
        assert_eq!(c.path, "/cgi-bin");
        assert!(!c.persistent, "expiry=0 means session cookie");
    }
}
