//! Mock LPD (Local Peer Discovery) server for E2E testing
//!
//! Provides a mock UDP multicast server that simulates LPD announcements
//! for testing BitTorrent local peer discovery functionality.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Standard LPD multicast address
#[allow(dead_code)]
pub const LPD_MULTICAST_ADDR: &str = "239.192.152.143";
/// Standard LPD port
#[allow(dead_code)]
pub const LPD_PORT: u16 = 6771;

/// Represents a peer announcement received by the mock LPD server
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LpdAnnouncement {
    /// The torrent info hash (40-char hex string)
    pub info_hash: String,
    /// The peer's listen port
    pub port: u16,
    /// The anti-spoofing token
    pub token: u32,
    /// The source IP address
    pub source_addr: IpAddr,
}

/// Mock LPD server that can send and receive LPD announcements
///
/// This server simulates the LPD multicast protocol for testing purposes.
/// It can:
/// - Receive announcements from LPD clients
/// - Send mock announcements to simulate peer discovery
/// - Track all received announcements for verification
pub struct MockLpdServer {
    /// UDP socket bound to LPD multicast group
    socket: Arc<UdpSocket>,
    /// Whether the server is running
    running: Arc<Mutex<bool>>,
    /// Received announcements (thread-safe)
    received: Arc<Mutex<Vec<LpdAnnouncement>>>,
    /// Unique info hashes we've seen
    seen_hashes: Arc<Mutex<HashSet<String>>>,
    /// Local port we're bound to
    port: u16,
}

impl MockLpdServer {
    /// Create a new mock LPD server
    ///
    /// Binds to an ephemeral port on localhost for testing.
    /// Note: For true multicast testing, you'd need to bind to the
    /// actual LPD multicast address, but for E2E tests we use localhost.
    #[allow(dead_code)]
    pub fn new() -> Result<Self, String> {
        // Bind to ephemeral port on localhost for testing
        // (Real LPD would use the multicast address)
        let socket = UdpSocket::bind("127.0.0.1:0")
            .map_err(|e| format!("Failed to bind socket: {}", e))?;

        socket
            .set_broadcast(true)
            .map_err(|e| format!("Failed to enable broadcast: {}", e))?;

        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| format!("Failed to set read timeout: {}", e))?;

        let port = socket
            .local_addr()
            .map_err(|e| format!("Failed to get local addr: {}", e))?
            .port();

        Ok(Self {
            socket: Arc::new(socket),
            running: Arc::new(Mutex::new(false)),
            received: Arc::new(Mutex::new(Vec::new())),
            seen_hashes: Arc::new(Mutex::new(HashSet::new())),
            port,
        })
    }

    /// Create a mock LPD server bound to a specific port
    #[allow(dead_code)]
    pub fn with_port(port: u16) -> Result<Self, String> {
        let socket = UdpSocket::bind(format!("127.0.0.1:{}", port))
            .map_err(|e| format!("Failed to bind socket: {}", e))?;

        socket
            .set_broadcast(true)
            .map_err(|e| format!("Failed to enable broadcast: {}", e))?;

        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| format!("Failed to set read timeout: {}", e))?;

        Ok(Self {
            socket: Arc::new(socket),
            running: Arc::new(Mutex::new(false)),
            received: Arc::new(Mutex::new(Vec::new())),
            seen_hashes: Arc::new(Mutex::new(HashSet::new())),
            port,
        })
    }

    /// Get the port the server is listening on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the server's address
    #[allow(dead_code)]
    pub fn addr(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .expect("Failed to get local address")
    }

    /// Start the mock server in a background thread
    ///
    /// The server will listen for incoming LPD announcements and
    /// record them for later verification.
    #[allow(dead_code)]
    pub fn start(&self) {
        let mut running = self.running.lock().unwrap();
        if *running {
            return;
        }
        *running = true;
        drop(running);

        let socket = self.socket.clone();
        let running = self.running.clone();
        let received = self.received.clone();
        let seen_hashes = self.seen_hashes.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 1024];

            loop {
                // Check if still running
                {
                    let r = running.lock().unwrap();
                    if !*r {
                        break;
                    }
                }

                match socket.recv_from(&mut buf) {
                    Ok((len, src_addr)) => {
                        if let Some(announcement) =
                            Self::parse_announcement(&buf[..len], src_addr.ip())
                        {
                            // Track unique hashes
                            {
                                let mut seen = seen_hashes.lock().unwrap();
                                seen.insert(announcement.info_hash.clone());
                            }

                            // Store announcement
                            {
                                let mut recv = received.lock().unwrap();
                                recv.push(announcement);
                            }
                        }
                    }
                    Err(_) => {
                        // Timeout or error, continue
                        continue;
                    }
                }
            }
        });
    }

    /// Stop the mock server
    pub fn stop(&self) {
        let mut running = self.running.lock().unwrap();
        *running = false;
    }

    /// Send an LPD announcement to a target address
    ///
    /// This simulates a peer announcing itself via LPD.
    #[allow(dead_code)]
    pub fn send_announcement(
        &self,
        info_hash: &str,
        port: u16,
        target: SocketAddr,
    ) -> Result<(), String> {
        let token: u32 = rand::random();
        let msg = format!("Hash: {}\nPort: {}\nToken: {:08x}\n", info_hash, port, token);

        self.socket
            .send_to(msg.as_bytes(), target)
            .map_err(|e| format!("Failed to send announcement: {}", e))?;

        Ok(())
    }

    /// Send an LPD announcement with a specific token
    #[allow(dead_code)]
    pub fn send_announcement_with_token(
        &self,
        info_hash: &str,
        port: u16,
        token: u32,
        target: SocketAddr,
    ) -> Result<(), String> {
        let msg = format!("Hash: {}\nPort: {}\nToken: {:08x}\n", info_hash, port, token);

        self.socket
            .send_to(msg.as_bytes(), target)
            .map_err(|e| format!("Failed to send announcement: {}", e))?;

        Ok(())
    }

    /// Get all received announcements
    #[allow(dead_code)]
    pub fn get_received(&self) -> Vec<LpdAnnouncement> {
        self.received.lock().unwrap().clone()
    }

    /// Get count of received announcements
    #[allow(dead_code)]
    pub fn received_count(&self) -> usize {
        self.received.lock().unwrap().len()
    }

    /// Get unique info hashes seen
    #[allow(dead_code)]
    pub fn unique_hashes(&self) -> HashSet<String> {
        self.seen_hashes.lock().unwrap().clone()
    }

    /// Clear all received announcements
    #[allow(dead_code)]
    pub fn clear_received(&self) {
        self.received.lock().unwrap().clear();
        self.seen_hashes.lock().unwrap().clear();
    }

    /// Parse an LPD announcement from raw bytes
    fn parse_announcement(data: &[u8], source_ip: IpAddr) -> Option<LpdAnnouncement> {
        let text = std::str::from_utf8(data).ok()?;
        let mut info_hash = String::new();
        let mut port = 0u16;
        let mut token: Option<u32> = None;

        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Hash:") {
                let val = rest.trim();
                if val.len() == 40 && val.chars().all(|c| c.is_ascii_hexdigit()) {
                    info_hash = val.to_lowercase();
                } else {
                    return None;
                }
            } else if let Some(rest) = line.strip_prefix("Port:") {
                port = rest.trim().parse().ok()?;
                if port == 0 {
                    return None;
                }
            } else if let Some(rest) = line.strip_prefix("Token:") {
                token = u32::from_str_radix(rest.trim(), 16).ok();
            }
        }

        if !info_hash.is_empty() && port > 0 && token.is_some() {
            Some(LpdAnnouncement {
                info_hash,
                port,
                token: token.unwrap(),
                source_addr: source_ip,
            })
        } else {
            None
        }
    }
}

impl Drop for MockLpdServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mock LPD peer that simulates a BitTorrent client announcing via LPD
///
/// This is a simpler helper for tests that just need to simulate
/// a peer announcing without running a full server.
#[allow(dead_code)]
pub struct MockLpdPeer {
    /// The peer's info hash
    pub info_hash: String,
    /// The peer's listen port
    pub port: u16,
    /// The peer's IP address
    pub addr: Ipv4Addr,
}

impl MockLpdPeer {
    /// Create a new mock LPD peer
    #[allow(dead_code)]
    pub fn new(info_hash: &str, port: u16, addr: Ipv4Addr) -> Self {
        Self {
            info_hash: info_hash.to_string(),
            port,
            addr,
        }
    }

    /// Create a mock LPD peer with a random info hash
    #[allow(dead_code)]
    pub fn random(port: u16) -> Self {
        let mut hash_bytes = [0u8; 20];
        for (i, b) in hash_bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(rand::random::<u8>());
        }
        let info_hash = hex::encode(hash_bytes);
        Self {
            info_hash,
            port,
            addr: Ipv4Addr::new(127, 0, 0, 1),
        }
    }

    /// Format an LPD announcement message
    pub fn format_announcement(&self, token: u32) -> String {
        format!(
            "Hash: {}\nPort: {}\nToken: {:08x}\n",
            self.info_hash, self.port, token
        )
    }

    /// Send an announcement to a target socket
    #[allow(dead_code)]
    pub fn announce_to(&self, socket: &UdpSocket, target: SocketAddr) -> Result<u32, String> {
        let token: u32 = rand::random();
        let msg = self.format_announcement(token);
        socket
            .send_to(msg.as_bytes(), target)
            .map_err(|e| format!("Failed to send: {}", e))?;
        Ok(token)
    }
}

/// Helper to create a valid 40-char hex info hash for testing
pub fn make_test_info_hash(seed: u8) -> String {
    let mut bytes = [0u8; 20];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = seed.wrapping_add(i as u8);
    }
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_lpd_server_creation() {
        let server = MockLpdServer::new().unwrap();
        assert!(server.port() > 0);
    }

    #[test]
    fn test_mock_lpd_peer_format() {
        let peer = MockLpdPeer::new(
            "0123456789abcdef0123456789abcdef01234567",
            6881,
            Ipv4Addr::new(192, 168, 1, 1),
        );

        let msg = peer.format_announcement(0xDEADBEEF);
        assert!(msg.contains("Hash: 0123456789abcdef0123456789abcdef01234567"));
        assert!(msg.contains("Port: 6881"));
        assert!(msg.contains("Token: deadbeef"));
    }

    #[test]
    fn test_make_test_info_hash() {
        let hash1 = make_test_info_hash(0x42);
        let hash2 = make_test_info_hash(0x42);
        assert_eq!(hash1, hash2, "Same seed should produce same hash");
        assert_eq!(hash1.len(), 40, "Info hash should be 40 chars");
        assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_parse_valid_announcement() {
        let data = b"Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\nToken: deadbeef\n";
        let result = MockLpdServer::parse_announcement(data, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));

        assert!(result.is_some());
        let ann = result.unwrap();
        assert_eq!(ann.info_hash, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(ann.port, 6881);
        assert_eq!(ann.token, 0xDEADBEEF);
    }

    #[test]
    fn test_parse_invalid_announcement() {
        // Missing token
        let data = b"Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\n";
        let result = MockLpdServer::parse_announcement(data, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(result.is_none());

        // Invalid hash length
        let data = b"Hash: short\nPort: 6881\nToken: deadbeef\n";
        let result = MockLpdServer::parse_announcement(data, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(result.is_none());

        // Port 0
        let data = b"Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 0\nToken: deadbeef\n";
        let result = MockLpdServer::parse_announcement(data, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(result.is_none());
    }
}