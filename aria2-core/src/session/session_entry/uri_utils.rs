//! URI utility functions for session serialization
//!
//! Provides URI escaping/unescaping and hex decoding used by the session
//! file format to safely encode URIs and bitfield data.

use crate::error::{Aria2Error, Result};

/// Escapes special characters in URIs for safe serialization.
pub fn escape_uri(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

/// Unescapes special characters previously escaped by [`escape_uri()`].
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

/// Decodes a hexadecimal string to a byte vector.
pub fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
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
