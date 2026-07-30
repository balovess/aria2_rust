use std::collections::BTreeMap;

/// Maximum nesting depth accepted by the decoder.
///
/// Mirrors the C++ `BencodeParser::pushState` guard (`stateStack_.size() >= 50`
/// -> `ERR_STRUCTURE_TOO_DEEP`). Without this limit a hostile `.torrent`
/// consisting of a long run of `l`/`d` opener bytes drives unbounded recursion
/// and aborts the process on stack overflow, which Rust cannot catch.
pub const MAX_NESTING_DEPTH: usize = 50;

#[derive(Debug, Clone, PartialEq)]
pub enum BencodeValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

impl BencodeValue {
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), String> {
        Self::decode_at_depth(bytes, 0)
    }

    fn decode_at_depth(bytes: &[u8], depth: usize) -> Result<(Self, usize), String> {
        if bytes.is_empty() {
            return Err("Empty byte stream".to_string());
        }

        match bytes[0] {
            b'i' => Self::decode_int(bytes),
            b'l' => Self::decode_list(bytes, depth),
            b'd' => Self::decode_dict(bytes, depth),
            b'0'..=b'9' => Self::decode_bytes(bytes),
            c => Err(format!(
                "Invalid bencode start character: '{}' (0x{:02x})",
                c as char, c
            )),
        }
    }

    /// Returns the depth a nested container's children live at, or an error when
    /// the structure would exceed [`MAX_NESTING_DEPTH`].
    fn descend(depth: usize) -> Result<usize, String> {
        if depth >= MAX_NESTING_DEPTH {
            return Err(format!(
                "Bencode structure too deep: exceeds maximum nesting depth of {}",
                MAX_NESTING_DEPTH
            ));
        }
        Ok(depth + 1)
    }

    fn decode_int(bytes: &[u8]) -> Result<(Self, usize), String> {
        if !bytes.starts_with(b"i") {
            return Err("Integer does not start with 'i'".to_string());
        }
        let end = bytes
            .iter()
            .position(|&b| b == b'e')
            .ok_or("Integer missing end marker 'e'")?;
        if end <= 1 {
            return Err("Integer is empty".to_string());
        }
        let num_str = std::str::from_utf8(&bytes[1..end])
            .map_err(|e| format!("Integer content is not valid UTF-8: {}", e))?;
        let value: i64 = num_str
            .parse()
            .map_err(|e| format!("Failed to parse integer: {} (content: '{}')", e, num_str))?;
        Ok((BencodeValue::Int(value), end + 1))
    }

    fn decode_bytes(bytes: &[u8]) -> Result<(Self, usize), String> {
        let colon_pos = bytes
            .iter()
            .position(|&b| b == b':')
            .ok_or("Byte string missing length separator ':'")?;
        if colon_pos == 0 {
            return Err("Byte string length is empty".to_string());
        }
        let len_str = std::str::from_utf8(&bytes[..colon_pos])
            .map_err(|e| format!("Byte string length prefix is not valid UTF-8: {}", e))?;
        let length: usize = len_str
            .parse()
            .map_err(|e| format!("Failed to parse byte string length: {}", e))?;
        let data_start = colon_pos + 1;
        // `length` comes straight off the wire; a value near `usize::MAX` would
        // wrap on a plain add and make the bounds check below pass, then panic
        // inside the slice index. Reject the overflow explicitly instead.
        let data_end = data_start.checked_add(length).ok_or_else(|| {
            format!(
                "Byte string length overflows address space: declared length={}",
                length
            )
        })?;
        if data_end > bytes.len() {
            return Err(format!(
                "Byte string data insufficient: declared length={}, available={}",
                length,
                bytes.len() - data_start
            ));
        }
        Ok((
            BencodeValue::Bytes(bytes[data_start..data_end].to_vec()),
            data_end,
        ))
    }

    fn decode_list(bytes: &[u8], depth: usize) -> Result<(Self, usize), String> {
        if !bytes.starts_with(b"l") {
            return Err("List does not start with 'l'".to_string());
        }
        let inner_depth = Self::descend(depth)?;
        let mut pos = 1;
        let mut items = Vec::new();
        while pos < bytes.len() && bytes[pos] != b'e' {
            let (item, consumed) = Self::decode_at_depth(&bytes[pos..], inner_depth)?;
            items.push(item);
            pos += consumed;
        }
        if pos >= bytes.len() {
            return Err("List missing end marker 'e'".to_string());
        }
        Ok((BencodeValue::List(items), pos + 1))
    }

    fn decode_dict(bytes: &[u8], depth: usize) -> Result<(Self, usize), String> {
        if !bytes.starts_with(b"d") {
            return Err("Dictionary does not start with 'd'".to_string());
        }
        let inner_depth = Self::descend(depth)?;
        let mut pos = 1;
        let mut entries = BTreeMap::new();
        while pos < bytes.len() && bytes[pos] != b'e' {
            let (key, key_consumed) = Self::decode_at_depth(&bytes[pos..], inner_depth)?;
            let key_bytes = match key {
                BencodeValue::Bytes(b) => b,
                _ => return Err("Dict key must be a byte string".to_string()),
            };
            pos += key_consumed;

            if pos >= bytes.len() || bytes[pos] == b'e' {
                return Err("Dict value missing (odd number of elements)".to_string());
            }
            let (value, val_consumed) = Self::decode_at_depth(&bytes[pos..], inner_depth)?;
            entries.insert(key_bytes, value);
            pos += val_consumed;
        }
        if pos >= bytes.len() {
            return Err("Dict missing end marker 'e'".to_string());
        }
        Ok((BencodeValue::Dict(entries), pos + 1))
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            BencodeValue::Int(n) => format!("i{}e", n).into_bytes(),
            BencodeValue::Bytes(data) => {
                let mut result = format!("{}:", data.len()).into_bytes();
                result.extend_from_slice(data);
                result
            }
            BencodeValue::List(items) => {
                let mut result = vec![b'l'];
                for item in items {
                    result.extend(item.encode());
                }
                result.push(b'e');
                result
            }
            BencodeValue::Dict(entries) => {
                let mut result = vec![b'd'];
                for (key, value) in entries {
                    result.extend(format!("{}:", key.len()).into_bytes());
                    result.extend_from_slice(key);
                    result.extend(value.encode());
                }
                result.push(b'e');
                result
            }
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            BencodeValue::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            BencodeValue::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn as_list(&self) -> Option<&Vec<BencodeValue>> {
        match self {
            BencodeValue::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, BencodeValue>> {
        match self {
            BencodeValue::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn dict_get<K: AsRef<[u8]>>(&self, key: K) -> Option<&BencodeValue> {
        self.as_dict()?.get(key.as_ref())
    }

    pub fn dict_get_str(&self, key: &str) -> Option<&str> {
        self.dict_get(key.as_bytes()).and_then(|v| v.as_str())
    }

    pub fn dict_get_int(&self, key: &str) -> Option<i64> {
        self.dict_get(key.as_bytes()).and_then(|v| v.as_int())
    }

    pub fn is_int(&self) -> bool {
        matches!(self, BencodeValue::Int(_))
    }
    pub fn is_bytes(&self) -> bool {
        matches!(self, BencodeValue::Bytes(_))
    }
    pub fn is_list(&self) -> bool {
        matches!(self, BencodeValue::List(_))
    }
    pub fn is_dict(&self) -> bool {
        matches!(self, BencodeValue::Dict(_))
    }
}

impl From<i64> for BencodeValue {
    fn from(n: i64) -> Self {
        BencodeValue::Int(n)
    }
}

impl From<Vec<u8>> for BencodeValue {
    fn from(b: Vec<u8>) -> Self {
        BencodeValue::Bytes(b)
    }
}

impl From<String> for BencodeValue {
    fn from(s: String) -> Self {
        BencodeValue::Bytes(s.into_bytes())
    }
}

impl From<Vec<BencodeValue>> for BencodeValue {
    fn from(l: Vec<BencodeValue>) -> Self {
        BencodeValue::List(l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_int() {
        let original = BencodeValue::Int(42);
        let encoded = original.encode();
        let (decoded, _) = BencodeValue::decode(&encoded).unwrap();
        assert_eq!(original, decoded);

        assert_eq!(BencodeValue::decode(b"i0e"), Ok((BencodeValue::Int(0), 3)));
        assert_eq!(
            BencodeValue::decode(b"i-42e"),
            Ok((BencodeValue::Int(-42), 5))
        );
        assert_eq!(
            BencodeValue::decode(b"i123456789e"),
            Ok((BencodeValue::Int(123456789), 11))
        );
    }

    #[test]
    fn test_encode_decode_bytes() {
        let original = BencodeValue::Bytes(b"hello".to_vec());
        let encoded = original.encode();
        let (decoded, _) = BencodeValue::decode(&encoded).unwrap();
        assert_eq!(original, decoded);

        assert_eq!(
            BencodeValue::decode(b"4:spam"),
            Ok((BencodeValue::Bytes(b"spam".to_vec()), 6))
        );
        assert_eq!(
            BencodeValue::decode(b"0:"),
            Ok((BencodeValue::Bytes(vec![]), 2))
        );
        let binary_data: Vec<u8> = vec![0, 1, 2, 255];
        let enc = BencodeValue::Bytes(binary_data.clone()).encode();
        assert_eq!(
            BencodeValue::decode(&enc),
            Ok((BencodeValue::Bytes(binary_data), 6))
        );
    }

    #[test]
    fn test_encode_decode_list() {
        let list = BencodeValue::List(vec![
            BencodeValue::Int(1),
            BencodeValue::Bytes(b"two".to_vec()),
            BencodeValue::Int(3),
        ]);
        let encoded = list.encode();
        let (decoded, _) = BencodeValue::decode(&encoded).unwrap();
        assert_eq!(list, decoded);

        assert_eq!(
            BencodeValue::decode(b"le"),
            Ok((BencodeValue::List(vec![]), 2))
        );
    }

    #[test]
    fn test_encode_decode_dict() {
        let mut map = BTreeMap::new();
        map.insert(b"bar".to_vec(), BencodeValue::Bytes(b"spam".to_vec()));
        map.insert(b"foo".to_vec(), BencodeValue::Int(42));
        let dict = BencodeValue::Dict(map);
        let encoded = dict.encode();
        let (decoded, _) = BencodeValue::decode(&encoded).unwrap();
        assert_eq!(dict, decoded);

        assert_eq!(
            BencodeValue::decode(b"de"),
            Ok((BencodeValue::Dict(BTreeMap::new()), 2))
        );
    }

    #[test]
    fn test_nested_structures() {
        let nested = BencodeValue::Dict({
            let mut m = BTreeMap::new();
            m.insert(
                b"a".to_vec(),
                BencodeValue::List(vec![BencodeValue::Dict({
                    let mut inner = BTreeMap::new();
                    inner.insert(b"x".to_vec(), BencodeValue::Int(99));
                    inner
                })]),
            );
            m
        });
        let encoded = nested.encode();
        let (decoded, _) = BencodeValue::decode(&encoded).unwrap();
        assert_eq!(nested, decoded);
    }

    #[test]
    fn test_type_accessors() {
        let v = BencodeValue::Int(100);
        assert_eq!(v.as_int(), Some(100));
        assert!(v.as_str().is_none());

        let v = BencodeValue::Bytes(b"hello".to_vec());
        assert_eq!(v.as_str(), Some("hello"));
        assert!(v.as_int().is_none());

        let mut d = BTreeMap::new();
        d.insert(b"key".to_vec(), BencodeValue::Int(42));
        let v = BencodeValue::Dict(d);
        assert_eq!(v.dict_get_int("key"), Some(42));
        assert!(v.dict_get_str("missing").is_none());
    }

    #[test]
    fn test_error_cases() {
        assert!(BencodeValue::decode(b"").is_err());
        assert!(BencodeValue::decode(b"ie").is_err());
        assert!(BencodeValue::decode(b"i").is_err());
        assert!(BencodeValue::decode(b":hello").is_err());
        assert!(BencodeValue::decode(b"5:hi").is_err());
        assert!(BencodeValue::decode(b"l").is_err());
        assert!(BencodeValue::decode(b"d").is_err());
        assert!(BencodeValue::decode(b"d3:key").is_err());
    }

    #[test]
    fn test_partial_decode() {
        let input = b"i42e4:test";
        let (val, consumed) = BencodeValue::decode(input).unwrap();
        assert_eq!(val, BencodeValue::Int(42));
        assert_eq!(consumed, 4);
        assert_eq!(&input[consumed..], b"4:test");
    }

    /// Builds `open`-repeated ... `e`-repeated, e.g. depth 3 with `l` => "llleee".
    fn nested(open: u8, depth: usize) -> Vec<u8> {
        let mut v = vec![open; depth];
        v.extend(std::iter::repeat(b'e').take(depth));
        v
    }

    #[test]
    fn test_nesting_at_limit_is_accepted() {
        let input = nested(b'l', MAX_NESTING_DEPTH);
        assert!(
            BencodeValue::decode(&input).is_ok(),
            "exactly MAX_NESTING_DEPTH levels must still parse"
        );
    }

    #[test]
    fn test_nesting_beyond_limit_is_rejected() {
        let input = nested(b'l', MAX_NESTING_DEPTH + 1);
        let err = BencodeValue::decode(&input).unwrap_err();
        assert!(err.contains("too deep"), "unexpected error: {err}");
    }

    #[test]
    fn test_dict_nesting_beyond_limit_is_rejected() {
        // d1:a d1:a ... i0e e...e  -- nest dictionaries through their values.
        let depth = MAX_NESTING_DEPTH + 5;
        let mut input = Vec::new();
        for _ in 0..depth {
            input.extend_from_slice(b"d1:a");
        }
        input.extend_from_slice(b"i0e");
        input.extend(std::iter::repeat(b'e').take(depth));
        let err = BencodeValue::decode(&input).unwrap_err();
        assert!(err.contains("too deep"), "unexpected error: {err}");
    }

    /// A hostile torrent used to abort the process via stack overflow here.
    #[test]
    fn test_pathological_nesting_does_not_overflow_stack() {
        let input = vec![b'l'; 200_000];
        let err = BencodeValue::decode(&input).unwrap_err();
        assert!(err.contains("too deep"), "unexpected error: {err}");
    }

    #[test]
    fn test_byte_string_length_overflow_is_rejected() {
        // `usize::MAX` as the length prefix wrapped the `data_start + length`
        // add and panicked inside the slice index before the checked_add fix.
        let input = format!("{}:", usize::MAX).into_bytes();
        let err = BencodeValue::decode(&input).unwrap_err();
        assert!(
            err.contains("overflow") || err.contains("insufficient"),
            "unexpected error: {err}"
        );
    }
}
