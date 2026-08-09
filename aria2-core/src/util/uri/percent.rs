//! Percent-encoding and percent-decoding helpers for URI components.
//!
//! Follows RFC 3986 for unreserved character set in encoding.
//! Decoding is lenient: invalid/incomplete sequences are left as-is.

/// Percent-encode a string for URI components (userinfo, etc.).
///
/// Encodes all bytes that are not in the unreserved set (RFC 3986 §2.3):
/// `ALPHA / DIGIT / "-" / "." / "_" / "~"`.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

/// Percent-decode a string (e.g. `%20` → space, `%E6%96%87` → 文).
///
/// Invalid/incomplete percent sequences are left as-is.
pub fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}
