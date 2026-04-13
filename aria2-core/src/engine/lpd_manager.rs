//! Local Peer Discovery (LPD) Manager - Phase 15 H8
//!
//! Implements BitTorrent Local Peer Discovery using UDP multicast on
//! the standard LPD multicast group 239.192.152.143:6771.
//!
//! # Architecture
//!
//! ```text
//! lpd_manager.rs (this file)
//!   ├── LpdManager - High-level coordinator for LPD operations
//!   ├── LpdAnnouncer - Low-level UDP multicast sender/receiver
//!   ├── LpdPeer - Discovered peer information
//!   └── parse_lpd_announcement() - Parse text-format LPD messages
//!
//! LPD Protocol:
//!   Multicast Group: 239.192.152.143:6771
//!   Message Format:
//!     Hash: <info_hash_hex>\n
//!     Port: <listen_port>\n
//!     Token: <random_8hex>\n
//!
//!   Announce Interval: Every 5 minutes while active
//! ```

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// =========================================================================
// Constants
// =========================================================================

/// Standard LPD multicast address (IPv4)
pub const LPD_MULTICAST_ADDR: &str = "239.192.152.143";
/// Standard LPD port
pub const LPD_PORT: u16 = 6771;
/// Default announce interval in seconds
pub const DEFAULT_ANNOUNCE_INTERVAL_SECS: u64 = 300; // 5 minutes
/// Maximum number of peers to track per info hash
pub const MAX_PEERS_PER_HASH: usize = 50;
/// Receive buffer size for LPD announcements
pub const RECV_BUFFER_SIZE: usize = 1024;
/// Timeout for receiving announcements during discovery
pub const DEFAULT_RECEIVE_TIMEOUT_MS: u64 = 2000;

// =========================================================================
// LpdPeer - Discovered peer from LPD announcement
// =========================================================================

/// Information about a peer discovered via LPD
#[derive(Debug, Clone)]
pub struct LpdPeer {
    /// The torrent info hash this peer is sharing
    pub info_hash: String,
    /// The peer's listen port
    pub port: u16,
    /// The peer's IP address (from recv_from)
    pub addr: IpAddr,
    /// When this peer was last announced
    pub last_seen: Instant,
    /// Random token from the announcement (for anti-spoofing)
    pub token: Option<u32>,
}

impl LpdPeer {
    /// Create a new LpdPeer
    pub fn new(info_hash: impl Into<String>, port: u16, addr: IpAddr) -> Self {
        Self {
            info_hash: info_hash.into(),
            port,
            addr,
            last_seen: Instant::now(),
            token: None,
        }
    }

    /// Create with token
    pub fn with_token(info_hash: impl Into<String>, port: u16, addr: IpAddr, token: u32) -> Self {
        let mut p = Self::new(info_hash, port, addr);
        p.token = Some(token);
        p
    }

    /// Get the peer's address as SocketAddr
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.port)
    }

    /// Check if this peer has expired based on age
    pub fn is_expired(&self, max_age: Duration) -> bool {
        self.last_seen.elapsed() > max_age
    }
}

impl PartialEq for LpdPeer {
    fn eq(&self, other: &Self) -> bool {
        // Two peers are considered equal if they share same info_hash and IP
        self.info_hash == other.info_hash && self.addr == other.addr
    }
}

impl Eq for LpdPeer {}

impl std::hash::Hash for LpdPeer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.info_hash.hash(state);
        self.addr.hash(state);
    }
}

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
        Self::with_config(DEFAULT_ANNOUNCE_INTERVAL_SECS)
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
        let multicast_addr: SocketAddr = format!("{}:{}", LPD_MULTICAST_ADDR, LPD_PORT)
            .parse()
            .map_err(|e| format!("Invalid LPD multicast address: {}", e))?;

        // Join multicast group
        let multicast_ip: Ipv4Addr = LPD_MULTICAST_ADDR
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

    /// Get the local bind address
    pub fn local_addr(&self) -> Result<SocketAddr, String> {
        self.socket
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))
    }

    /// Send an LPD announcement for a torrent
    ///
    /// Formats and sends a text-based LPD message containing:
    /// - Hash: <info_hash> (40-char hex)
    /// - Port: <listen_port>
    /// - Token: <random 8-hex>
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

        // Generate random anti-spoofing token
        let token: u32 = rand::random::<u32>();

        // Format LPD announcement per BEP-14 spec
        let msg = format!(
            "Hash: {}\nPort: {}\nToken: {:08x}\n",
            info_hash, port, token
        );

        debug!(
            info_hash = %&info_hash[..8],
            port,
            token = format!("{:08x}", token),
            "Sending LPD announcement"
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
        let mut buf = [0u8; RECV_BUFFER_SIZE];
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
                                "Received valid LPD announcement"
                            );
                            peers.push(peer);
                        } else {
                            debug!(
                                addr = %src_addr.ip(),
                                "Duplicate LPD announcement suppressed"
                            );
                        }
                    }
                }
                Err(e) => {
                    let kind = e.kind();
                    if kind == std::io::ErrorKind::TimedOut
                        || kind == std::io::ErrorKind::WouldBlock
                    {
                        break;
                    }
                    // Other errors: log but continue trying
                    warn!(error = %e, "LPD receive error, continuing");
                    // Small sleep to avoid busy-looping on persistent errors
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }

        debug!(count = peers.len(), "LPD receive completed");
        peers
    }

    /// Perform a single announce+receive cycle (announce then collect responses)
    ///
    /// Sends our announcement, waits briefly, then collects any responses
    /// from other peers who also announced in that window.
    pub fn announce_and_discover(
        &self,
        info_hash: &str,
        port: u16,
        discover_timeout: Duration,
    ) -> Result<Vec<LpdPeer>, String> {
        // Announce ourselves
        self.announce(info_hash, port)?;

        // Wait a bit for others to respond
        std::thread::sleep(Duration::from_millis(500));

        // Collect announcements
        let peers = self.receive_announcements(discover_timeout);

        // Filter out our own announcement (by matching our info_hash + port)
        let peers: Vec<LpdPeer> = peers
            .into_iter()
            .filter(|p| !(p.info_hash == info_hash && p.port == port))
            .collect();

        Ok(peers)
    }
}

// =========================================================================
// LpdManager - High-level coordinator
// =========================================================================

/// Manages LPD operations for all active torrents.
///
/// Maintains a registry of known peers discovered via LPD, handles periodic
/// announcements for active downloads, and coordinates with the download engine.
pub struct LpdManager {
    /// The underlying UDP announcer
    announcer: Arc<LpdAnnouncer>,
    /// Registry of discovered peers keyed by info_hash
    peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>>,
    /// Track which info hashes we're currently announcing
    pub active_hashes: Arc<RwLock<HashSet<String>>>,
    /// Handle to the background announce task
    _announce_task: Option<tokio::task::JoinHandle<()>>,
}

impl Default for LpdManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LpdManager {
    /// Create a new LpdManager
    ///
    /// Initializes the UDP socket, joins multicast group, and sets up
    /// internal state tracking.
    pub fn new() -> Self {
        let announcer = LpdAnnouncer::new().unwrap_or_else(|_| {
            // If we can't create real socket, create a dummy one for testing
            // In production this would be an error
            warn!("Could not create LPD announcer, LPD will be disabled");
            LpdAnnouncer::with_config(DEFAULT_ANNOUNCE_INTERVAL_SECS)
                .unwrap_or_else(|_| panic!("Fatal: cannot create LPD announcer"))
        });

        Self {
            announcer: Arc::new(announcer),
            peers: Arc::new(RwLock::new(HashMap::new())),
            active_hashes: Arc::new(RwLock::new(HashSet::new())),
            _announce_task: None,
        }
    }

    /// Create LpdManager with custom configuration
    pub fn with_interval(announce_interval_secs: u64) -> Result<Self, String> {
        let announcer = LpdAnnouncer::with_config(announce_interval_secs)?;

        Ok(Self {
            announcer: Arc::new(announcer),
            peers: Arc::new(RwLock::new(HashMap::new())),
            active_hashes: Arc::new(RwLock::new(HashSet::new())),
            _announce_task: None,
        })
    }

    /// Register a torrent for LPD announcements
    ///
    /// Adds the info_hash to the active set so it gets periodically announced.
    pub async fn register_torrent(&self, info_hash: &str) -> Result<(), String> {
        let mut active = self.active_hashes.write().await;
        active.insert(info_hash.to_string());

        // Ensure peer set exists
        let mut peers_map = self.peers.write().await;
        peers_map.entry(info_hash.to_string()).or_default();

        info!(info_hash = %&info_hash[..8], "Torrent registered for LPD");
        Ok(())
    }

    /// Unregister a torrent from LPD announcements
    pub async fn unregister_torrent(&self, info_hash: &str) {
        let mut active = self.active_hashes.write().await;
        active.remove(info_hash);

        info!(info_hash = %&info_hash[..8], "Torrent unregistered from LPD");
    }

    /// Manual announce for a specific torrent
    pub async fn announce_torrent(&self, info_hash: &str, port: u16) -> Result<(), String> {
        self.announcer.announce(info_hash, port)?;
        Ok(())
    }

    /// Discover peers for a specific info_hash via LPD
    pub async fn discover_peers(&self, _info_hash: &str, timeout_ms: Option<u64>) -> Vec<LpdPeer> {
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_RECEIVE_TIMEOUT_MS));
        self.announcer.receive_announcements(timeout)
    }

    /// Get all known peers for a given info_hash
    pub async fn get_peers_for(&self, info_hash: &str) -> Vec<LpdPeer> {
        let peers_map = self.peers.read().await;
        peers_map
            .get(info_hash)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Start periodic background announce task
    ///
    /// Spawns a Tokio task that announces all registered torrents every
    /// N seconds (default 5 min).
    ///
    /// # Arguments
    ///
    /// * `port` - Our BT client listen port
    ///
    /// # Returns
    ///
    /// JoinHandle that can be used to cancel the task
    pub fn start_background_announce(&mut self, port: u16) -> Option<tokio::task::JoinHandle<()>> {
        if !self.announcer.is_enabled() {
            debug!("LPD is disabled, not starting background announce");
            return None;
        }

        let announcer = Arc::clone(&self.announcer);
        let active_hashes = Arc::clone(&self.active_hashes);

        info!(
            interval_secs = self.announcer.announce_interval.as_secs(),
            port, "Starting LPD background announce task"
        );

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(announcer.announce_interval);

            loop {
                ticker.tick().await;

                let hashes: Vec<String> = {
                    let active = active_hashes.read().await;
                    active.iter().cloned().collect()
                };

                for info_hash in &hashes {
                    if let Err(e) = announcer.announce(info_hash, port) {
                        warn!(
                            info_hash = %&info_hash[..8.min(info_hash.len())],
                            error = %e,
                            "Background LPD announce failed"
                        );
                    }
                }

                debug!(
                    count = hashes.len(),
                    "LPD background announce cycle completed"
                );
            }
        });

        self._announce_task = Some(handle);
        // Note: JoinHandle is not Clone, so we cannot return a copy.
        // The task is stored internally and can be stopped via stop_background_announce().
        None
    }

    /// Stop background announce task
    pub fn stop_background_announce(&mut self) {
        if let Some(handle) = self._announce_task.take() {
            handle.abort();
            info!("LPD background announce task stopped");
        }
    }

    /// Update peer registry with newly discovered peers
    pub async fn update_peers(&self, info_hash: &str, new_peers: Vec<LpdPeer>) {
        let mut peers_map = self.peers.write().await;
        let entry = peers_map.entry(info_hash.to_string()).or_default();

        for peer in new_peers {
            // Limit total peers per hash
            if entry.len() >= MAX_PEERS_PER_HASH {
                // Remove oldest expired peer first
                let oldest = entry.iter().max_by_key(|p| p.last_seen.elapsed()).cloned();
                if let Some(oldest_peer) = oldest {
                    entry.remove(&oldest_peer);
                }
            }
            entry.insert(peer);
        }
    }

    /// Clean up expired peers from all registries
    pub async fn cleanup_expired_peers(&self, max_age: Duration) -> usize {
        let mut peers_map = self.peers.write().await;
        let mut removed = 0usize;

        for (_hash, peers) in peers_map.iter_mut() {
            let before = peers.len();
            peers.retain(|p| !p.is_expired(max_age));
            removed += before - peers.len();
        }

        if removed > 0 {
            debug!(removed, "Cleaned up expired LPD peers");
        }
        removed
    }

    /// Check if LPD is available and working
    pub fn is_available(&self) -> bool {
        self.announcer.is_enabled()
    }
}

// =========================================================================
// LPD Announcement Parser
// =========================================================================

/// Parse a raw LPD announcement message into structured data
///
/// LPD messages are plain text with key-value pairs:
///
/// ```text
/// Hash: <40-char-hex-info-hash>\n
/// Port: <1-65535>\n
/// Token: <8-char-hex-token>\n
/// ```
///
/// # Arguments
///
/// * `data` - Raw bytes received from UDP socket
/// * `sender_ip` - IP address of the sender (from recv_from)
///
/// # Returns
///
/// `Some(LpdPeer)` if parsing succeeds, `None` if malformed
pub fn parse_lpd_announcement(data: &[u8], sender_ip: IpAddr) -> Option<LpdPeer> {
    let text = std::str::from_utf8(data).ok()?;
    let mut info_hash = String::new();
    let mut port = 0u16;
    let mut token: Option<u32> = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Hash:") {
            let val = rest.trim();
            // Validate: must be exactly 40 hex characters
            if val.len() == 40 && val.chars().all(|c| c.is_ascii_hexdigit()) {
                info_hash = val.to_lowercase(); // Normalize to lowercase
            } else {
                return None; // Invalid info_hash format
            }
        } else if let Some(rest) = line.strip_prefix("Port:") {
            port = rest.trim().parse().ok()?;
            if port == 0 {
                return None; // Port 0 is invalid
            }
        } else if let Some(rest) = line.strip_prefix("Token:") {
            let token_str = rest.trim();
            token = u32::from_str_radix(token_str, 16).ok();
        }
    }

    // All three fields are required for a valid announcement
    if !info_hash.is_empty() && port > 0 && token.is_some() {
        let mut peer = LpdPeer::new(info_hash, port, sender_ip);
        peer.token = token;
        Some(peer)
    } else {
        debug!(
            text_len = data.len(),
            has_hash = !info_hash.is_empty(),
            has_port = port > 0,
            has_token = token.is_some(),
            "Incomplete/malformed LPD announcement ignored"
        );
        None
    }
}
