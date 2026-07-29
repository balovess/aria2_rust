//! LpdAnnouncer - UDP multicast sender/receiver for LPD announcements.
//!
//! Handles low-level UDP socket binding, multicast group joining, and
//! BEP 14 announcement send/receive operations.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::constants;

use super::discovery::parse_lpd_announcement;
use super::LpdPeer;

// =========================================================================
// LpdAnnouncer - UDP Multicast Sender/Receiver
// =========================================================================

/// Handles low-level UDP multicast I/O for LPD announcements.
///
/// Binds to a local UDP socket, joins the LPD multicast group, and provides
/// methods for sending announcements and receiving peer discoveries.
///
/// # Thread Safety
///
/// `LpdAnnouncer` uses `UdpSocket` which is `Send + Sync`. However, concurrent
/// send/recv calls may need external synchronization for correctness.
pub struct LpdAnnouncer {
    /// Bound UDP socket for multicast I/O
    socket: UdpSocket,
    /// The multicast address we send to / receive from
    multicast_addr: SocketAddr,
    /// Whether announcing is enabled
    enabled: bool,
    /// Current announce interval
    announce_interval: Duration,
}

impl LpdAnnouncer {
    /// Create a new LpdAnnouncer bound to an ephemeral local port
    ///
    /// Joins the LPD multicast group (239.192.152.143:6771) and enables
    /// broadcast mode on the socket.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Cannot bind to any local UDP port
    /// - Cannot enable broadcast mode
    /// - Cannot join multicast group
    pub fn new() -> Result<Self, String> {
        Self::with_config(constants::LPD_DEFAULT_ANNOUNCE_INTERVAL_SECS)
    }

    /// Create with custom announce interval
    pub fn with_config(announce_interval_secs: u64) -> Result<Self, String> {
        // Bind to ephemeral port on all interfaces
        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| format!("Failed to bind UDP socket: {}", e))?;

        // Enable broadcast
        socket
            .set_broadcast(true)
            .map_err(|e| format!("Failed to enable broadcast: {}", e))?;

        // Set reuse address so multiple instances can coexist (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_REUSEADDR,
                    &1i32 as *const i32 as *const libc::c_void,
                    std::mem::size_of::<i32>() as libc::socklen_t,
                );
            }
        }

        // Parse multicast address
        let multicast_addr: SocketAddr = format!(
            "{}:{}",
            constants::LPD_MULTICAST_ADDRESS,
            constants::LPD_PORT
        )
        .parse()
        .map_err(|e| format!("Invalid LPD multicast address: {}", e))?;

        // Join multicast group
        let multicast_ip: Ipv4Addr = constants::LPD_MULTICAST_ADDRESS
            .parse()
            .map_err(|e| format!("Invalid LPD multicast IP: {}", e))?;

        socket
            .join_multicast_v4(&multicast_ip, &Ipv4Addr::UNSPECIFIED)
            .map_err(|e| format!("Failed to join LPD multicast group: {}", e))?;

        debug!(
            local = ?socket.local_addr().ok(),
            multicast = %multicast_addr,
            "LpdAnnouncer created successfully"
        );

        Ok(Self {
            socket,
            multicast_addr,
            enabled: true,
            announce_interval: Duration::from_secs(announce_interval_secs),
        })
    }

    /// Disable announcing (for testing or when BT is disabled)
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    /// Enable announcing
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// Check if announcer is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get the current announce interval duration
    pub fn announce_interval(&self) -> Duration {
        self.announce_interval
    }

    /// Get the local bind address
    pub fn local_addr(&self) -> Result<SocketAddr, String> {
        self.socket
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))
    }

    /// Send an LPD announcement for a torrent
    ///
    /// Formats and sends a BEP 14 compliant LPD message:
    ///
    /// ```http
    /// BT-SEARCH * HTTP/1.1\r\n
    /// Host: 239.192.152.143:6771\r\n
    /// Port: <listen_port>\r\n
    /// Infohash: <40-char-hex>\r\n
    /// \r\n\r\n
    /// ```
    ///
    /// This matches the C++ `bittorrent::createLpdRequest()` format exactly,
    /// ensuring interoperability with other BEP 14 clients (libtorrent,
    /// qBittorrent, transmission, original aria2, etc.).
    ///
    /// # Arguments
    ///
    /// * `info_hash` - 40-character hex string of the torrent's info hash
    /// * `port` - Our listening port for incoming connections
    ///
    /// # Errors
    ///
    /// Returns error if UDP send fails.
    pub fn announce(&self, info_hash: &str, port: u16) -> Result<(), String> {
        if !self.enabled {
            return Ok(()); // Silently succeed when disabled
        }

        // Validate info hash format (should be 40 hex chars)
        if info_hash.len() != 40 || !info_hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "Invalid info_hash format: expected 40 hex chars, got {} chars",
                info_hash.len()
            ));
        }

        // Format LPD announcement per BEP 14 spec.
        // C++: `fmt("BT-SEARCH * HTTP/1.1\r\nHost: %s:%u\r\nPort: %u\r\nInfohash: %s\r\n\r\n\r\n", ...)`
        let msg = format!(
            "BT-SEARCH * HTTP/1.1\r\nHost: {}:{}\r\nPort: {}\r\nInfohash: {}\r\n\r\n\r\n",
            constants::LPD_MULTICAST_ADDRESS,
            constants::LPD_PORT,
            port,
            info_hash
        );

        debug!(
            info_hash = %&info_hash[..8],
            port,
            "Sending BEP14 LPD announcement"
        );

        self.socket
            .send_to(msg.as_bytes(), self.multicast_addr)
            .map_err(|e| format!("LPD announce send failed: {}", e))?;

        Ok(())
    }

    /// Receive LPD announcements within a timeout window
    ///
    /// Blocks for up to `timeout` duration, collecting all valid LPD
    /// announcements received. Deduplicates by (info_hash, source_ip).
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum time to wait for announcements
    ///
    /// # Returns
    ///
    /// Vector of discovered peers. May be empty if no announcements received.
    pub fn receive_announcements(&self, timeout: Duration) -> Vec<LpdPeer> {
        let mut buf = [0u8; constants::LPD_RECEIVE_BUFFER_SIZE];
        let mut peers = Vec::new();
        let mut seen: HashSet<(String, IpAddr)> = HashSet::new();

        self.socket
            .set_read_timeout(Some(timeout))
            .expect("set_read_timeout should not fail");

        let deadline = Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }

            match self.socket.recv_from(&mut buf) {
                Ok((len, src_addr)) => {
                    if len == 0 {
                        continue;
                    }

                    if let Some(peer) = parse_lpd_announcement(&buf[..len], src_addr.ip()) {
                        // Deduplicate by (info_hash, ip)
                        let key = (peer.info_hash.clone(), peer.addr);
                        if seen.insert(key) {
                            debug!(
                                info_hash = %peer.info_hash[..8.min(peer.info_hash.len())],
                                addr = %src_addr.ip(),
                port = peer.port,
                                "Discovered peer via LPD"
                            );
                            peers.push(peer);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Timeout reached, no more data
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "LPD receive error");
                    break;
                }
            }
        }

        peers
    }
}
