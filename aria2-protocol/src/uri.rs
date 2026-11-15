//! Shared URI component encoding helpers.

/// Percent-decode valid escapes while preserving malformed escapes verbatim.
pub fn percent_decode(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut index = 0;
    let input = value.as_bytes();

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

/// Percent-encode a URI component using the RFC 3986 unreserved set.
pub fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                use std::fmt::Write as _;
                write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
    }
    encoded
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{percent_decode, percent_encode};

    #[test]
    fn percent_encoding_round_trips_uri_components() {
        let original = "user@name:pa%ss /文件";
        assert_eq!(percent_decode(&percent_encode(original)), original);
    }

    #[test]
    fn percent_decode_preserves_plus_and_malformed_escapes() {
        assert_eq!(percent_decode("a+b%20c%ZZ%3"), "a+b c%ZZ%3");
    }
}
