//! SFTP URI percent-decoding utilities.
//!
//! This mirrors `aria2_original`'s `util::percentDecode`: valid `%XX`
//! sequences become one byte, while malformed sequences remain unchanged.
//! The same decoder is used for SFTP userinfo and paths so their URI semantics
//! cannot drift.

/// Decode percent escapes without treating `+` specially.
pub fn sftp_percent_decode(input: &str) -> String {
    let input = input.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        if input[index] == b'%'
            && let (Some(high), Some(low)) = (
                input.get(index + 1).copied().and_then(hex_value),
                input.get(index + 2).copied().and_then(hex_value),
            )
        {
            bytes.push((high << 4) | low);
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
