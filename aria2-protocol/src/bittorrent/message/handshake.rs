use super::types::{HANDSHAKE_LENGTH, PROTOCOL_STRING};

// Reserved byte constants per the BitTorrent protocol specifications.
// The reserved field is 8 bytes (indices 0..7).
//
// | Feature                 | BEP  | Bit position          | Value    |
// |-------------------------|------|------------------------|----------|
// | DHT                     | 5    | reserved[7] bit 0     | 0x01     |
// | Fast Extension          | 6    | reserved[7] bit 2     | 0x04     |
// | Extended Messaging      | 10   | reserved[5] bit 4     | 0x10     |
// | MSE (libtorrent conv.)  | n/a  | reserved[7] bit 0*   | 0x01     |
//
// * MSE shares the same bit as DHT in some implementations; C++ aria2
//   does NOT set an MSE bit by default — it only *checks* for it.

/// DHT: reserved[7] bit 0 (BEP 5)
const RESERVED_DHT: u8 = 0x01;
/// Fast Extension: reserved[7] bit 2 (BEP 6)
const RESERVED_FAST_EXT: u8 = 0x04;
/// Extended Messaging: reserved[5] bit 4 (BEP 10)
const RESERVED_EXT_MSG: u8 = 0x10;
/// BitTorrent v2 hybrid upgrade support (BEP 52), the fourth most
/// significant bit of the final reserved byte.
const RESERVED_BEP52: u8 = 0x10;

#[derive(Debug, Clone)]
pub struct Handshake {
    pub protocol: [u8; 19],
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

impl Handshake {
    /// Create a new handshake with standard extension bits set.
    ///
    /// Mirrors C++ `BtHandshakeMessage::init()`:
    /// - `reserved[7] |= 0x04` — Fast Extension (BEP 6)
    /// - `reserved[5] |= 0x10` — Extended Messaging (BEP 10)
    ///
    /// DHT is **not** set by default; the caller must call
    /// [`set_dht_enabled(true)`](Self::set_dht_enabled) to advertise DHT
    /// support (C++ calls `setDHTEnabled()` separately based on config).
    pub fn new(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Self {
        let mut reserved = [0u8; 8];
        // Fast Extension (BEP 6)
        reserved[7] |= RESERVED_FAST_EXT;
        // Extended Messaging (BEP 10)
        reserved[5] |= RESERVED_EXT_MSG;

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

    /// Enable or disable the DHT reserved bit (BEP 5).
    ///
    /// Mirrors C++ `setDHTEnabled(bool)`:
    /// - `reserved[7] |= 0x01` when enabled
    /// - `reserved[7] &= ~0x01` when disabled
    pub fn set_dht_enabled(&mut self, enabled: bool) {
        if enabled {
            self.reserved[7] |= RESERVED_DHT;
        } else {
            self.reserved[7] &= !RESERVED_DHT;
        }
    }

    /// Builder-pattern version of [`set_dht_enabled`].
    pub fn with_dht(mut self, enabled: bool) -> Self {
        self.set_dht_enabled(enabled);
        self
    }

    /// Enable or disable the BEP 52 hybrid upgrade capability.
    pub fn set_bep52_enabled(&mut self, enabled: bool) {
        if enabled {
            self.reserved[7] |= RESERVED_BEP52;
        } else {
            self.reserved[7] &= !RESERVED_BEP52;
        }
    }

    /// Builder-pattern version of [`set_bep52_enabled`].
    pub fn with_bep52(mut self, enabled: bool) -> Self {
        self.set_bep52_enabled(enabled);
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
                "Insufficient handshake data: need {} bytes, got {}",
                HANDSHAKE_LENGTH,
                data.len()
            ));
        }

        let pstrlen = data[0] as usize;
        if pstrlen != 19 {
            return Err(format!("Invalid protocol string length: {}", pstrlen));
        }

        let protocol = {
            let mut arr = [0u8; 19];
            arr.copy_from_slice(&data[1..20]);
            arr
        };

        if protocol != PROTOCOL_STRING {
            return Err(format!(
                "Unsupported protocol: {}",
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

        Ok(Self {
            protocol,
            reserved,
            info_hash,
            peer_id,
        })
    }

    /// Check if the peer supports MSE (Message Stream Encryption).
    ///
    /// NOTE: There is no standard BEP for MSE reserved bits. C++ aria2 does
    /// **not** set any MSE bit by default. This check follows the libtorrent
    /// convention of checking `reserved[7] & 0x01`, which overlaps with DHT.
    /// In practice, MSE is detected via the encryption handshake itself,
    /// not via reserved bits.
    pub fn supports_mse(&self) -> bool {
        (self.reserved[7] & RESERVED_DHT) != 0
    }

    /// Check if the peer supports DHT (BEP 5).
    ///
    /// Mirrors C++ `isDHTEnabled()`: `reserved[7] & 0x01`.
    pub fn supports_dht(&self) -> bool {
        (self.reserved[7] & RESERVED_DHT) != 0
    }

    /// Check if the peer supports Fast Extension (BEP 6).
    ///
    /// Mirrors C++ `isFastExtensionSupported()`: `reserved[7] & 0x04`.
    pub fn supports_fast_extension(&self) -> bool {
        (self.reserved[7] & RESERVED_FAST_EXT) != 0
    }

    /// Check if the peer supports Extended Messaging (BEP 10).
    ///
    /// Mirrors C++ `isExtendedMessagingEnabled()`: `reserved[5] & 0x10`.
    pub fn supports_extended_messaging(&self) -> bool {
        (self.reserved[5] & RESERVED_EXT_MSG) != 0
    }

    /// Check whether the peer supports the BEP 52 hybrid upgrade path.
    pub fn supports_bep52(&self) -> bool {
        (self.reserved[7] & RESERVED_BEP52) != 0
    }

    pub fn peer_id_str(&self) -> String {
        self.peer_id.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn peer_id_readable(&self) -> Option<String> {
        std::str::from_utf8(&self.peer_id)
            .ok()
            .map(|s| s.to_string())
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
        assert!(parsed.supports_fast_extension());
        assert!(parsed.supports_extended_messaging());
        // DHT is NOT set by default — must be explicitly enabled
        assert!(!parsed.supports_dht());
    }

    #[test]
    fn test_handshake_with_dht() {
        let hs = Handshake::new(&[3u8; 20], &[4u8; 20]).with_dht(true);
        let bytes = hs.to_bytes();
        let parsed = Handshake::parse(&bytes).unwrap();
        assert!(parsed.supports_dht());
        assert!(parsed.supports_fast_extension());
        assert!(parsed.supports_extended_messaging());
    }

    #[test]
    fn test_handshake_dht_disabled() {
        let mut hs = Handshake::new(&[3u8; 20], &[4u8; 20]);
        hs.set_dht_enabled(true);
        assert!(hs.supports_dht());
        hs.set_dht_enabled(false);
        assert!(!hs.supports_dht());
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
        b"-AR0001-"
            .iter()
            .enumerate()
            .for_each(|(i, &b)| pid[i] = b);
        let hs = Handshake::new(&[0u8; 20], &pid);
        assert!(hs.peer_id_readable().unwrap().starts_with("-AR"));
        assert_eq!(hs.peer_id_str().len(), 40);
    }

    #[test]
    fn test_reserved_bytes_fast_extension() {
        let hs = Handshake::new(&[0xAB; 20], &[0xCD; 20]);
        let bytes = hs.to_bytes();
        // reserved[7] should have Fast Extension bit set (0x04)
        assert_eq!(bytes[27] & 0x04, 0x04);
        // reserved[5] should have Extended Messaging bit set (0x10)
        assert_eq!(bytes[25] & 0x10, 0x10);
        // DHT not set by default
        assert_eq!(bytes[27] & 0x01, 0x00);
    }

    #[test]
    fn test_reserved_bytes_dht_enabled() {
        let hs = Handshake::new(&[0xAB; 20], &[0xCD; 20]).with_dht(true);
        let bytes = hs.to_bytes();
        // reserved[7] should have both DHT (0x01) and Fast Extension (0x04)
        assert_eq!(bytes[27] & 0x01, 0x01);
        assert_eq!(bytes[27] & 0x04, 0x04);
    }

    #[test]
    fn test_reserved_bytes_bep52_enabled() {
        let hs = Handshake::new(&[0xAB; 20], &[0xCD; 20]).with_bep52(true);
        let parsed = Handshake::parse(&hs.to_bytes()).unwrap();
        assert!(parsed.supports_bep52());
        assert_eq!(parsed.reserved[7] & RESERVED_BEP52, RESERVED_BEP52);
        assert_eq!(parsed.reserved[7] & RESERVED_FAST_EXT, RESERVED_FAST_EXT);
    }

    #[test]
    fn test_c_plus_plus_compatibility() {
        // Verify that the reserved bytes match C++ aria2-next exactly:
        //   reserved_[7] |= 0x04u;  // fast extension
        //   reserved_[5] |= 0x10u;  // extended messaging
        //   reserved_[7] |= 0x01u;  // DHT (when enabled)
        let mut hs = Handshake::new(&[0; 20], &[0; 20]);
        hs.set_dht_enabled(true);
        let bytes = hs.to_bytes();

        // reserved[5] should be 0x10 (Extended Messaging only)
        assert_eq!(bytes[25], 0x10);
        // reserved[7] should be 0x05 (DHT | Fast Extension = 0x01 | 0x04)
        assert_eq!(bytes[27], 0x05);
    }
}
