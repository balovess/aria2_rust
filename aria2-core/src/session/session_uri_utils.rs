//! Session URI and Hex utility functions
//!
//! Provides safe URI escaping/unescaping for session file format,
//! plus hexadecimal encoding/decoding for BitTorrent bitfield data.
//!
//! # URI Escaping Rules
//!
//! Special characters in URIs are escaped as follows:
//! - `\` → `\\`
//! - Tab (`\t`) → `\t`
//! - Newline (`\n`) → `\n`
//!
//! # Examples
//!
//! ```rust
//! use aria2_core::session::session_uri_utils::{escape_uri, unescape_uri};
//!
//! let escaped = escape_uri("path\\to\tfile\nname");
//! assert_eq!(escaped, "path\\\\to\\tfile\\nname");
//! assert_eq!(unescape_uri(&escaped), "path\\to\tfile\nname");
//! ```

use crate::error::{Aria2Error, Result};

/// Escapes special characters in URIs for safe serialization
///
/// Replaces special characters with escape sequences:
/// - `\` → `\\`
/// - `\t` (tab) → `\t`
/// - `\n` (newline) → `\n`
///
/// # Arguments
///
/// * `s` - Input string to escape
///
/// # Returns
///
/// Escaped string safe for inclusion in session file format
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_uri_utils::{escape_uri, unescape_uri};
///
/// let escaped = escape_uri("path\\to\tfile\nname");
/// assert_eq!(escaped, "path\\\\to\\tfile\\nname");
/// ```
pub fn escape_uri(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

/// Unescapes special characters previously escaped by [`escape_uri()`]
///
/// Processes escape sequences:
/// - `\\` → `\`
/// - `\t` → tab character
/// - `\n` → newline character
///
/// # Arguments
///
/// * `s` - Escaped input string
///
/// # Returns
///
/// Unescaped original string
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_uri_utils::{escape_uri, unescape_uri};
///
/// assert_eq!(unescape_uri(&escape_uri("hello\tworld")), "hello\tworld");
/// assert_eq!(unescape_uri(&escape_uri("line1\nline2")), "line1\nline2");
/// assert_eq!(unescape_uri(&escape_uri("back\\slash")), "back\\slash");
/// ```
pub fn unescape_uri(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                match next {
                    't' => {
                        result.push('\t');
                        chars.next();
                    }
                    'n' => {
                        result.push('\n');
                        chars.next();
                    }
                    '\\' => {
                        result.push('\\');
                        chars.next();
                    }
                    _ => {
                        result.push(c);
                    }
                }
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Decodes a hexadecimal string to a byte vector
///
/// Converts a string of hex characters (e.g., "ff00ff") into
/// the corresponding bytes ([0xFF, 0x00, xFF]).
///
/// # Arguments
///
/// * `hex` - Hexadecimal string (must have even length)
///
/// # Returns
///
/// Result containing the decoded byte vector or an error if:
/// - The hex string has odd length
/// - Contains invalid hex characters
///
/// # Errors
///
/// Returns [`Aria2Error::Io`] if the hex string is malformed
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_uri_utils::decode_hex;
///
/// let bytes = decode_hex("fff00f").unwrap();
/// assert_eq!(bytes, vec![0xFF, 0xF0, 0x0F]);
/// ```
pub fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err(Aria2Error::Io(format!(
            "Hex string has odd length: {}",
            hex.len()
        )));
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);

    for i in (0..hex.len()).step_by(2) {
        let byte_str = &hex[i..i + 2];
        let byte = u8::from_str_radix(byte_str, 16).map_err(|e| {
            Aria2Error::Io(format!("Invalid hex character at position {}: {}", i, e))
        })?;
        bytes.push(byte);
    }

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_unescape_uri() {
        assert_eq!(unescape_uri(&escape_uri("hello\tworld")), "hello\tworld");
        assert_eq!(unescape_uri(&escape_uri("line1\nline2")), "line1\nline2");
        assert_eq!(unescape_uri(&escape_uri("back\\slash")), "back\\slash");
        assert_eq!(unescape_uri(&escape_uri("normal")), "normal");
    }

    #[test]
    fn test_decode_hex_valid() {
        // Test valid hex strings
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode_hex("00").unwrap(), vec![0x00]);
        assert_eq!(decode_hex("ff").unwrap(), vec![0xFF]);
        assert_eq!(decode_hex("fff00f").unwrap(), vec![0xFF, 0xF0, 0x0F]);
        assert_eq!(
            decode_hex("deadbeef").unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn test_decode_hex_invalid() {
        // Test invalid hex strings
        assert!(decode_hex("abc").is_err()); // Odd length
        assert!(decode_hex("ghij").is_err()); // Invalid characters
    }
}
