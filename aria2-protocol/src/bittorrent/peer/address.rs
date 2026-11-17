//! BitTorrent peer endpoint values and compact endpoint representations.

#[derive(Debug, Clone, PartialEq)]
pub struct PeerAddr {
    pub ip: String,
    pub port: u16,
}

impl PeerAddr {
    pub fn new(ip: &str, port: u16) -> Self {
        Self {
            ip: ip.to_string(),
            port,
        }
    }

    /// Compact peer format sizes for IPv4 and IPv6.
    pub const COMPACT_SIZE_V4: usize = 6;
    pub const COMPACT_SIZE_V6: usize = 18;

    /// Decode from IPv4 compact format (4-byte IP + 2-byte port = 6 bytes).
    pub fn from_compact(data: &[u8]) -> Option<Self> {
        if data.len() < Self::COMPACT_SIZE_V4 {
            return None;
        }
        let ip = format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3]);
        let port = u16::from_be_bytes([data[4], data[5]]);
        Some(Self { ip, port })
    }

    /// Decode from IPv6 compact format (16-byte IP + 2-byte port = 18 bytes).
    pub fn from_compact_v6(data: &[u8]) -> Option<Self> {
        if data.len() < Self::COMPACT_SIZE_V6 {
            return None;
        }
        let ip_bytes: [u8; 16] = data[..16].try_into().ok()?;
        let ipv6 = std::net::Ipv6Addr::from(ip_bytes);
        let port = u16::from_be_bytes([data[16], data[17]]);
        Some(Self {
            ip: ipv6.to_string(),
            port,
        })
    }

    pub fn to_socket_addr(&self) -> Result<std::net::SocketAddr, std::net::AddrParseError> {
        self.ip
            .parse()
            .map(|ip| std::net::SocketAddr::new(ip, self.port))
    }

    /// Encode to IPv4 compact format (4-byte IP + 2-byte port = 6 bytes).
    pub fn to_compact_v4(&self) -> Option<[u8; 6]> {
        let addr = self.ip.parse::<std::net::Ipv4Addr>().ok()?;
        let mut buf = [0u8; 6];
        buf[..4].copy_from_slice(&addr.octets());
        buf[4..].copy_from_slice(&self.port.to_be_bytes());
        Some(buf)
    }

    /// Legacy IPv4 compact encoder that substitutes an all-zero endpoint for
    /// invalid address strings.
    #[deprecated(note = "use to_compact_v4 to handle invalid addresses explicitly")]
    pub fn to_compact(&self) -> [u8; 6] {
        self.to_compact_v4().unwrap_or([0; 6])
    }

    /// Encode to IPv6 compact format (16-byte IP + 2-byte port = 18 bytes).
    pub fn to_compact_v6(&self) -> Option<[u8; 18]> {
        let addr = self.ip.parse::<std::net::Ipv6Addr>().ok()?;
        let mut buf = [0u8; 18];
        buf[..16].copy_from_slice(&addr.octets());
        buf[16..].copy_from_slice(&self.port.to_be_bytes());
        Some(buf)
    }
}

/// Decode a complete list of IPv4 compact peer endpoints.
///
/// Returns `None` when the byte length is not a multiple of six. An empty
/// list is valid and returns `Some(Vec::new())`.
pub fn decode_compact_v4_peers(data: &[u8]) -> Option<Vec<std::net::SocketAddr>> {
    if !data.len().is_multiple_of(PeerAddr::COMPACT_SIZE_V4) {
        return None;
    }

    let (chunks, _) = data.as_chunks::<6>();
    Some(
        chunks
            .iter()
            .map(|chunk| {
                let ip = std::net::Ipv4Addr::from([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                std::net::SocketAddr::new(ip.into(), port)
            })
            .collect(),
    )
}

/// Decode a complete list of IPv6 compact peer endpoints.
///
/// Returns `None` when the byte length is not a multiple of eighteen. An
/// empty list is valid and returns `Some(Vec::new())`.
pub fn decode_compact_v6_peers(data: &[u8]) -> Option<Vec<std::net::SocketAddr>> {
    if !data.len().is_multiple_of(PeerAddr::COMPACT_SIZE_V6) {
        return None;
    }

    let (chunks, _) = data.as_chunks::<18>();
    Some(
        chunks
            .iter()
            .map(|chunk| {
                let mut octets = [0; 16];
                octets.copy_from_slice(&chunk[..16]);
                let ip = std::net::Ipv6Addr::from(octets);
                let port = u16::from_be_bytes([chunk[16], chunk[17]]);
                std::net::SocketAddr::new(ip.into(), port)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{decode_compact_v4_peers, decode_compact_v6_peers};

    #[test]
    fn decodes_complete_compact_peer_lists_and_rejects_trailing_bytes() {
        assert_eq!(
            decode_compact_v4_peers(&[192, 0, 2, 1, 0x1a, 0xe1]),
            Some(vec!["192.0.2.1:6881".parse().unwrap()])
        );
        assert_eq!(decode_compact_v4_peers(&[]), Some(Vec::new()));
        assert_eq!(decode_compact_v4_peers(&[1]), None);

        assert_eq!(
            decode_compact_v6_peers(&[
                0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x1a, 0xe1,
            ]),
            Some(vec!["[2001:db8::1]:6881".parse().unwrap()])
        );
        assert_eq!(decode_compact_v6_peers(&[]), Some(Vec::new()));
        assert_eq!(decode_compact_v6_peers(&[1]), None);
    }
}
