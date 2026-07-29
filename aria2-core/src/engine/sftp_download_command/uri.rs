//! SFTP URI path decoding utilities.
//!
//! Provides percent-encoding decoding for SFTP paths, including correct
//! handling of multi-byte UTF-8 sequences (e.g. CJK characters).

/// Decode a percent-encoded SFTP path, handling UTF-8 multi-byte sequences correctly.
///
/// For example, `"%E6%96%87%E4%BB%B6"` decodes to the Chinese characters for "file".
pub fn sftp_path_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
            } else {
                // Invalid percent-encoding, push literal characters
                bytes.extend_from_slice(c.to_string().as_bytes());
                bytes.extend_from_slice(hex.as_bytes());
            }
        } else {
            bytes.push(c as u8);
        }
    }
    // Decode the full byte sequence as UTF-8, with lossy fallback for invalid sequences
    String::from_utf8_lossy(&bytes).into_owned()
}
