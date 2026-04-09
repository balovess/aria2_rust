use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum BencodeValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

impl BencodeValue {
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), String> {
        if bytes.is_empty() {
            return Err("空字节流".to_string());
        }

        match bytes[0] {
            b'i' => Self::decode_int(bytes),
            b'l' => Self::decode_list(bytes),
            b'd' => Self::decode_dict(bytes),
            b'0'..=b'9' => Self::decode_bytes(bytes),
            c => Err(format!(
                "无效的bencode起始字符: '{}' (0x{:02x})",
                c as char, c
            )),
        }
    }

    fn decode_int(bytes: &[u8]) -> Result<(Self, usize), String> {
        if !bytes.starts_with(b"i") {
            return Err("整数不以'i'开头".to_string());
        }
        let end = bytes
            .iter()
            .position(|&b| b == b'e')
            .ok_or("整数缺少结束标记'e'")?;
        if end <= 1 {
            return Err("整数为空".to_string());
        }
        let num_str = unsafe { std::str::from_utf8_unchecked(&bytes[1..end]) };
        let value: i64 = num_str
            .parse()
            .map_err(|e| format!("解析整数失败: {} (内容: '{}')", e, num_str))?;
        Ok((BencodeValue::Int(value), end + 1))
    }

    fn decode_bytes(bytes: &[u8]) -> Result<(Self, usize), String> {
        let colon_pos = bytes
            .iter()
            .position(|&b| b == b':')
            .ok_or("字节串缺少长度分隔符':'")?;
        if colon_pos == 0 {
            return Err("字节串长度为空".to_string());
        }
        let len_str = unsafe { std::str::from_utf8_unchecked(&bytes[..colon_pos]) };
        let length: usize = len_str
            .parse()
            .map_err(|e| format!("解析字节串长度失败: {}", e))?;
        let data_start = colon_pos + 1;
        let data_end = data_start + length;
        if data_end > bytes.len() {
            return Err(format!(
                "字节串数据不足: 声明长度={}, 实际可用={}",
                length,
                bytes.len() - data_start
            ));
        }
        Ok((
            BencodeValue::Bytes(bytes[data_start..data_end].to_vec()),
            data_end,
        ))
    }

    fn decode_list(bytes: &[u8]) -> Result<(Self, usize), String> {
        if !bytes.starts_with(b"l") {
            return Err("列表不以'l'开头".to_string());
        }
        let mut pos = 1;
        let mut items = Vec::new();
        while pos < bytes.len() && bytes[pos] != b'e' {
            let (item, consumed) = Self::decode(&bytes[pos..])?;
            items.push(item);
            pos += consumed;
        }
        if pos >= bytes.len() {
            return Err("列表缺少结束标记'e'".to_string());
        }
        Ok((BencodeValue::List(items), pos + 1))
    }

    fn decode_dict(bytes: &[u8]) -> Result<(Self, usize), String> {
        if !bytes.starts_with(b"d") {
            return Err("字典不以'd'开头".to_string());
        }
        let mut pos = 1;
        let mut entries = BTreeMap::new();
        while pos < bytes.len() && bytes[pos] != b'e' {
            let (key, key_consumed) = Self::decode(&bytes[pos..])?;
            let key_bytes = match key {
                BencodeValue::Bytes(b) => b,
                _ => return Err("字典键必须是字节串".to_string()),
            };
            pos += key_consumed;

            if pos >= bytes.len() || bytes[pos] == b'e' {
                return Err("字典值缺失(奇数个元素)".to_string());
            }
            let (value, val_consumed) = Self::decode(&bytes[pos..])?;
            entries.insert(key_bytes, value);
            pos += val_consumed;
        }
        if pos >= bytes.len() {
            return Err("字典缺少结束标记'e'".to_string());
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
}
