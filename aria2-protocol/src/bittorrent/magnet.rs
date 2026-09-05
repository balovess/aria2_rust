#[derive(Debug, Clone)]
pub struct MagnetLink {
    /// The v1 SHA-1 info hash, when the magnet carries `btih`.
    pub info_hash_v1: Option<[u8; 20]>,
    /// The 20-byte hash used by the BitTorrent wire protocol and DHT.
    /// For v2-only magnets this is the first 20 bytes of the SHA-256 info hash.
    pub info_hash: [u8; 20],
    /// The complete v2 SHA-256 info hash, when the magnet carries `btmh`.
    pub info_hash_v2: Option<[u8; 32]>,
    pub display_name: Option<String>,
    pub trackers: Vec<String>,
    pub exact_length: Option<u64>,
    pub ws: Vec<String>,
}

impl MagnetLink {
    pub fn parse(uri: &str) -> Result<Self, String> {
        let uri = uri.trim();
        if !uri.to_lowercase().starts_with("magnet:?") {
            return Err("Not a magnet link".to_string());
        }

        let query_part = &uri[8..];
        let mut info_hash = None;
        let mut info_hash_v1 = None;
        let mut info_hash_v2 = None;
        let mut display_name = None;
        let mut trackers = Vec::new();
        let mut exact_length = None;
        let mut ws = Vec::new();

        for pair in query_part.split('&') {
            if pair.is_empty() {
                continue;
            }
            let pair_str: &str = pair;
            let (key, value) = if let Some(pos) = pair_str.find('=') {
                (&pair_str[..pos], &pair_str[pos + 1..])
            } else {
                (pair_str, "")
            };

            match key.to_lowercase().as_str() {
                "xt" => {
                    let decoded = Self::url_decode(value);
                    match Self::extract_info_hash(&decoded)? {
                        Some(hash) => {
                            info_hash = Some(hash);
                            info_hash_v1 = Some(hash);
                        }
                        None => {
                            if let Some(hash) = Self::extract_info_hash_v2(&decoded)? {
                                info_hash_v2 = Some(hash);
                            }
                        }
                    }
                }
                "dn" => {
                    display_name = Some(Self::url_decode(value));
                }
                "tr" => {
                    trackers.push(Self::url_decode(value));
                }
                "xl" => {
                    if let Ok(size) = value.parse::<u64>() {
                        exact_length = Some(size);
                    }
                }
                "ws" => {
                    ws.push(Self::url_decode(value));
                }
                _ => {}
            }
        }

        let info_hash = info_hash
            .or_else(|| info_hash_v2.map(|hash| hash[..20].try_into().expect("32-byte hash")))
            .ok_or("Missing xt parameter (info hash)")?;

        Ok(Self {
            info_hash,
            info_hash_v1,
            info_hash_v2,
            display_name,
            trackers,
            exact_length,
            ws,
        })
    }

    fn extract_info_hash(xt: &str) -> Result<Option<[u8; 20]>, String> {
        let xt_lower = xt.to_lowercase();
        if !xt_lower.starts_with("urn:btih:") {
            return Ok(None);
        }

        let hash_str = &xt[9..];
        if hash_str.len() == 40 {
            let bytes = (0..40)
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(&hash_str[i..i + 2], 16)
                        .map_err(|e| format!("Invalid hex: {}", e))
                })
                .collect::<Result<Vec<u8>, _>>()?;
            if bytes.len() == 20 {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(&bytes);
                return Ok(Some(arr));
            }
        }

        if hash_str.len() == 32 {
            let decoded = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, hash_str)
                .ok_or("Invalid base32 hash")?;
            if decoded.len() == 20 {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(&decoded);
                return Ok(Some(arr));
            }
        }

        Err("Invalid info hash length".to_string())
    }

    fn extract_info_hash_v2(xt: &str) -> Result<Option<[u8; 32]>, String> {
        let xt_lower = xt.to_ascii_lowercase();
        if !xt_lower.starts_with("urn:btmh:") {
            return Ok(None);
        }

        // BEP 52 magnets use the SHA-256 multihash: 0x12 (sha2-256),
        // 0x20 (32-byte digest), followed by 64 hexadecimal characters.
        let hash_str = &xt[9..];
        if hash_str.len() != 68 || !hash_str[..4].eq_ignore_ascii_case("1220") {
            return Err("Invalid v2 multihash (expected 1220 + 64 hex characters)".to_string());
        }
        let bytes = (4..68)
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&hash_str[i..i + 2], 16)
                    .map_err(|e| format!("Invalid v2 hex: {}", e))
            })
            .collect::<Result<Vec<u8>, _>>()?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Some(arr))
    }

    pub fn url_decode(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%' {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte as char);
                } else {
                    result.push(c);
                    result.push_str(&hex);
                }
            } else if c == '+' {
                result.push(' ');
            } else {
                result.push(c);
            }
        }
        result
    }

    pub fn info_hash_hex(&self) -> String {
        self.info_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    pub fn info_hash_v2_hex(&self) -> Option<String> {
        self.info_hash_v2.map(hex::encode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_magnet_with_hex_hash() {
        let magnet = "magnet:?xt=urn:btih:3b245e04703a1ec5c91cef3f2295ee88ab63c50d&dn=Ubuntu+22.04&tr=udp://tracker.example.com:1337/announce";
        let ml = MagnetLink::parse(magnet).unwrap();
        assert_eq!(
            ml.info_hash_hex(),
            "3b245e04703a1ec5c91cef3f2295ee88ab63c50d"
        );
        assert_eq!(ml.display_name, Some("Ubuntu 22.04".to_string()));
        assert_eq!(ml.trackers.len(), 1);
        assert!(ml.info_hash_v2.is_none());
    }

    #[test]
    fn test_parse_magnet_minimal() {
        let magnet = "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678";
        let ml = MagnetLink::parse(magnet).unwrap();
        assert!(ml.display_name.is_none());
        assert!(ml.trackers.is_empty());
    }

    #[test]
    fn test_parse_invalid_not_magnet() {
        assert!(MagnetLink::parse("http://example.com").is_err());
    }

    #[test]
    fn test_parse_missing_xt() {
        assert!(MagnetLink::parse("magnet:?dn=test").is_err());
    }

    #[test]
    fn test_url_decode_spaces() {
        assert_eq!(MagnetLink::url_decode("Hello%20World"), "Hello World");
        assert_eq!(
            MagnetLink::url_decode("name+with+spaces"),
            "name with spaces"
        );
    }

    #[test]
    fn test_exact_length() {
        let magnet = "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&xl=1500000000";
        let ml = MagnetLink::parse(magnet).unwrap();
        assert_eq!(ml.exact_length, Some(1500000000));
    }

    #[test]
    fn test_parse_v2_magnet() {
        let magnet = "magnet:?xt=urn:btmh:122000112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let ml = MagnetLink::parse(magnet).unwrap();
        assert_eq!(
            ml.info_hash_v2,
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                0xcc, 0xdd, 0xee, 0xff
            ])
        );
        assert_eq!(
            ml.info_hash,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff, 0x00, 0x11, 0x22, 0x33
            ]
        );
    }

    #[test]
    fn test_parse_hybrid_magnet_and_encoded_v2() {
        let magnet = "magnet:?xt=urn:btih:3b245e04703a1ec5c91cef3f2295ee88ab63c50d&xt=urn%3Abtmh%3A122000112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let ml = MagnetLink::parse(magnet).unwrap();
        assert_eq!(
            ml.info_hash_hex(),
            "3b245e04703a1ec5c91cef3f2295ee88ab63c50d"
        );
        assert!(ml.info_hash_v2.is_some());
    }

    #[test]
    fn test_reject_invalid_v2_magnet() {
        assert!(MagnetLink::parse("magnet:?xt=urn:btmh:1221abcdef").is_err());
        assert!(MagnetLink::parse("magnet:?xt=urn:btmh:1220zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }
}
