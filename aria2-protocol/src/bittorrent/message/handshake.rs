use super::types::{HANDSHAKE_LENGTH, PROTOCOL_STRING};

const RESERVED_MSE: u8 = 0x01;
const RESERVED_DHT: u8 = 0x02;

#[derive(Debug, Clone)]
pub struct Handshake {
    pub protocol: [u8; 19],
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

impl Handshake {
    pub fn new(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Self {
        let mut reserved = [0u8; 8];
        reserved[5] |= RESERVED_DHT;
        let protocol: [u8; 19] = {
            let mut arr = [0u8; 19];
            arr.copy_from_slice(PROTOCOL_STRING);
            arr
        };
        Self {
            protocol,
            reserved,
            info_hash: *info_hash,
            peer_id: *peer_id,
        }
    }

    pub fn with_extensions(mut self, mse: bool) -> Self {
        if mse {
            self.reserved[0] |= RESERVED_MSE;
        }
        self
    }

    pub fn to_bytes(&self) -> [u8; HANDSHAKE_LENGTH] {
        let mut bytes = [0u8; HANDSHAKE_LENGTH];
        bytes[0] = PROTOCOL_STRING.len() as u8;
        bytes[1..20].copy_from_slice(PROTOCOL_STRING);
        bytes[20..28].copy_from_slice(&self.reserved);
        bytes[28..48].copy_from_slice(&self.info_hash);
        bytes[48..68].copy_from_slice(&self.peer_id);
        bytes
    }

    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < HANDSHAKE_LENGTH {
            return Err(format!(
                "握手数据长度不足: 需要{}字节, 实际{}字节",
                HANDSHAKE_LENGTH,
                data.len()
            ));
        }

        let pstrlen = data[0] as usize;
        if pstrlen != 19 {
            return Err(format!("无效的协议字符串长度: {}", pstrlen));
        }

        let protocol = {
            let mut arr = [0u8; 19];
            arr.copy_from_slice(&data[1..20]);
            arr
        };

        if &protocol != PROTOCOL_STRING {
            return Err(format!(
                "不支持的协议: {}",
                std::str::from_utf8(&protocol).unwrap_or("invalid")
            ));
        }

        let reserved = {
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&data[20..28]);
            arr
        };

        let info_hash = {
            let mut hash = [0u8; 20];
            hash.copy_from_slice(&data[28..48]);
            hash
        };

        let peer_id = {
            let mut id = [0u8; 20];
            id.copy_from_slice(&data[48..68]);
            id
        };

        Ok(Self { protocol, reserved, info_hash, peer_id })
    }

    pub fn supports_mse(&self) -> bool {
        (self.reserved[0] & RESERVED_MSE) != 0
    }

    pub fn supports_dht(&self) -> bool {
        (self.reserved[5] & RESERVED_DHT) != 0
    }

    pub fn peer_id_str(&self) -> String {
        self.peer_id.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn peer_id_readable(&self) -> Option<String> {
        std::str::from_utf8(&self.peer_id).ok().map(|s| s.to_string())
    }
}

impl PartialEq for Handshake {
    fn eq(&self, other: &Self) -> bool {
        self.info_hash == other.info_hash && self.peer_id == other.peer_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handshake_roundtrip() {
        let info_hash = [1u8; 20];
        let peer_id = [2u8; 20];

        let hs = Handshake::new(&info_hash, &peer_id);
        let bytes = hs.to_bytes();
        assert_eq!(bytes.len(), HANDSHAKE_LENGTH);

        let parsed = Handshake::parse(&bytes).unwrap();
        assert_eq!(parsed.info_hash, info_hash);
        assert_eq!(parsed.peer_id, peer_id);
        assert!(!parsed.supports_mse());
        assert!(parsed.supports_dht());
    }

    #[test]
    fn test_handshake_with_mse() {
        let hs = Handshake::new(&[3u8; 20], &[4u8; 20]).with_extensions(true);
        let bytes = hs.to_bytes();
        let parsed = Handshake::parse(&bytes).unwrap();
        assert!(parsed.supports_mse());
        assert!(parsed.supports_dht());
    }

    #[test]
    fn test_handshake_parse_error() {
        assert!(Handshake::parse(&[]).is_err());
        assert!(Handshake::parse(&[0; 67]).is_err());

        let mut bad_protocol = [0u8; HANDSHAKE_LENGTH];
        bad_protocol[0] = 19;
        bad_protocol[1..20].copy_from_slice(b"BadProtocol!!!!!!!!");
        assert!(Handshake::parse(&bad_protocol).is_err());
    }

    #[test]
    fn test_peer_id_string() {
        let mut pid = [0u8; 20];
        b"-AR0001-".iter().enumerate().for_each(|(i, &b)| pid[i] = b);
        let hs = Handshake::new(&[0u8; 20], &pid);
        assert!(hs.peer_id_readable().unwrap().starts_with("-AR"));
        assert_eq!(hs.peer_id_str().len(), 40);
    }

    #[test]
    fn test_reserved_bytes_preserved() {
        let hs = Handshake::new(&[0xAB; 20], &[0xCD; 20]);
        let bytes = hs.to_bytes();
        assert_eq!(bytes[20], 0x00);
        assert_eq!(bytes[25], 0x02);
    }
}
