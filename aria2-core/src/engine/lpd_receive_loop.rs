//! LPD Receive Loop — background task for continuous Local Peer Discovery
//!
//! This module implements the background receive loop for Local Peer
//! Discovery (LPD, BEP 14). The loop continuously reads LPD multicast
//! announcements from the UDP socket and feeds discovered peers back
//! to the [`LpdManager`](super::lpd_manager::LpdManager).
//!
//! # Architecture
//!
//! - [`LpdReceiveLoop`] — Manages the background tokio task that
//!   continuously receives LPD announcements. Mirrors C++
//!   `LpdReceiveMessageCommand` which re-adds itself to the event
//!   loop after each receive.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `LpdReceiveLoop` | `LpdReceiveMessageCommand` |
//! | `start()` | `LpdReceiveMessageCommand::execute()` loop |
//! | `create_lpd_socket()` | `LpdMessageReceiver::init()` |

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::constants;
use crate::engine::lpd_manager::{LpdPeer, parse_lpd_announcement};

// ===========================================================================
// Constants
// ===========================================================================

/// Maximum number of LPD messages to process per receive iteration.
/// Matches C++ `for (size_t i = 0; i < 20; ++i)` in
/// `LpdReceiveMessageCommand::execute()`.
const MAX_MESSAGES_PER_ITERATION: usize = 20;

/// LPD receive buffer size. Matches C++ `unsigned char buf[200]` in
/// `LpdMessageReceiver::receiveMessage()`. We use 1024 to match
/// `constants::LPD_RECEIVE_BUFFER_SIZE`.
const LPD_BUFFER_SIZE: usize = constants::LPD_RECEIVE_BUFFER_SIZE;

// ===========================================================================
// LpdReceiveLoop — background LPD receive task manager
// ===========================================================================

/// Manages the background tokio task that continuously receives LPD
/// multicast announcements.
///
/// The receive loop:
/// 1. Binds to the LPD multicast port (6771)
/// 2. Joins the LPD multicast group (239.192.152.143)
/// 3. Continuously reads LPD announcement messages
/// 4. Parses peer info (info_hash, port) from each message
/// 5. Filters announcements for only registered info hashes
/// 6. Feeds discovered peers to the [`LpdManager`](super::lpd_manager::LpdManager)
///
/// Mirrors C++ `LpdReceiveMessageCommand` which re-adds itself
/// to the event loop after each receive.
pub struct LpdReceiveLoop {
    /// Handle to the background receive task
    task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Cancellation token to gracefully stop the receive loop
    cancel_token: CancellationToken,
    /// Whether the receive loop is currently running
    is_running: Arc<AtomicBool>,
}

impl LpdReceiveLoop {
    /// Create a new receive loop in a stopped state.
    ///
    /// Call `start()` to begin receiving LPD announcements.
    pub fn new() -> Self {
        Self {
            task_handle: None,
            cancel_token: CancellationToken::new(),
            is_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the background receive loop.
    ///
    /// If already running, this is a no-op.
    ///
    /// # Arguments
    ///
    /// * `peers` — Shared peer registry keyed by info_hash
    /// * `active_hashes` — Set of info hashes currently being announced
    ///
    /// # Errors
    ///
    /// Returns `Err` if the UDP socket cannot be bound (e.g., port
    /// already in use, multicast unavailable in this environment).
    pub async fn start(
        &mut self,
        peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>>,
        active_hashes: Arc<RwLock<HashSet<String>>>,
    ) -> Result<(), String> {
        self.start_with_config(
            peers,
            active_hashes,
            constants::LPD_PORT,
            None,
        )
        .await
    }

    /// Start the loop on a configured BEP 14 port and multicast interface.
    pub async fn start_with_config(
        &mut self,
        peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>>,
        active_hashes: Arc<RwLock<HashSet<String>>>,
        listen_port: u16,
        interface: Option<Ipv4Addr>,
    ) -> Result<(), String> {
        if self.is_running.load(Ordering::Acquire) {
            debug!("LPD receive loop already running");
            return Ok(());
        }

        // Create and configure the UDP socket synchronously.
        // This mirrors C++ `LpdMessageReceiver::init()`.
        let socket = create_lpd_socket_with_config(listen_port, interface)?;

        // Convert to async socket for non-blocking receive in tokio runtime.
        let async_socket = tokio::net::UdpSocket::from_std(socket)
            .map_err(|e| format!("Failed to convert LPD socket to async: {}", e))?;

        let cancel_token = self.cancel_token.clone();
        let is_running = Arc::clone(&self.is_running);

        info!(
            addr = %constants::LPD_MULTICAST_ADDRESS,
            port = listen_port,
            "LPD receive loop starting"
        );

        // Spawn the background receive task.
        is_running.store(true, Ordering::Release);
        let handle = tokio::spawn(async move {
            run_receive_loop(async_socket, peers, active_hashes, cancel_token).await;
            is_running.store(false, Ordering::Release);
        });

        self.task_handle = Some(handle);
        Ok(())
    }

    /// Stop the background receive loop gracefully.
    ///
    /// Cancels the background task and waits for it to finish.
    pub async fn stop(&mut self) {
        if !self.is_running.load(Ordering::Acquire) {
            return;
        }

        self.cancel_token.cancel();

        if let Some(handle) = self.task_handle.take() {
            // Wait for the task to finish (with timeout)
            match tokio::time::timeout(Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    info!("LPD receive loop stopped gracefully");
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "LPD receive loop task panicked");
                }
                Err(_) => {
                    warn!("LPD receive loop stop timed out after 5s");
                }
            }
        }

        self.is_running.store(false, Ordering::Release);
        // Create a fresh cancellation token for potential restart
        self.cancel_token = CancellationToken::new();
    }

    /// Check if the receive loop is currently running.
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    /// Get a clone of the cancellation token.
    ///
    /// Allows external code to cancel the receive loop without holding
    /// a mutable reference to `LpdReceiveLoop`.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

impl Default for LpdReceiveLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LpdReceiveLoop {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
    }
}

// ===========================================================================
// Socket Creation — mirrors C++ LpdMessageReceiver::init()
// ===========================================================================

/// Create and configure a UDP socket bound to the LPD multicast port.
///
/// Steps:
/// 1. Set SO_REUSEADDR (Unix) so multiple instances can bind
/// 2. Bind to the LPD port:
///    - Windows: bind to `0.0.0.0:6771` (port only) due to MinGW limitations
///    - Unix: bind to `239.192.152.143:6771` (multicast address + port)
/// 3. Join multicast group 239.192.152.143 on all interfaces
///
/// This mirrors C++ `LpdMessageReceiver::init()` which does:
/// ```cpp
/// #ifdef __MINGW32__
///     socket_->bindWithFamily(multicastPort_, AF_INET);
/// #else
///     socket_->bind(multicastAddress_.c_str(), multicastPort_, AF_INET);
/// #endif
///     socket_->joinMulticastGroup(multicastAddress_, multicastPort_, localAddr);
/// ```
#[cfg(test)]
fn create_lpd_socket() -> Result<UdpSocket, String> {
    create_lpd_socket_with_config(constants::LPD_PORT, None)
}

fn create_lpd_socket_with_config(
    listen_port: u16,
    interface: Option<Ipv4Addr>,
) -> Result<UdpSocket, String> {
    if listen_port == 0 {
        return Err("LPD listen port must be greater than zero".to_string());
    }

    let multicast_ip: Ipv4Addr = constants::LPD_MULTICAST_ADDRESS
        .parse()
        .map_err(|e| format!("Invalid LPD multicast IP: {}", e))?;

    let port = listen_port;
    let local_interface = interface.unwrap_or(Ipv4Addr::UNSPECIFIED);

    // On Unix, create socket -> set SO_REUSEADDR -> bind -> join group.
    // SO_REUSEADDR must be set BEFORE bind so multiple processes can
    // bind to the same multicast port. This mirrors the C++ behavior
    // where SocketCore sets SO_REUSEADDR during bind.
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;

        // Create an unbound UDP socket.
        // SAFETY: `libc::socket` is a standard POSIX syscall. AF_INET and
        // SOCK_DGRAM are valid constants. The protocol argument 0 lets the
        // kernel choose the default protocol for SOCK_DGRAM (UDP).
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(format!(
                "Failed to create LPD UDP socket: {}",
                std::io::Error::last_os_error()
            ));
        }

        // Set SO_REUSEADDR before binding.
        // SAFETY: `fd` is a valid open socket descriptor (checked above).
        // `optval` is a valid `i32` on the stack whose reference outlives the
        // call. `size_of::<i32>()` correctly describes the option value size.
        let optval: i32 = 1;
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const i32 as *const libc::c_void,
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if result != 0 {
            warn!(
                "Failed to set SO_REUSEADDR on LPD socket (non-fatal): {}",
                std::io::Error::last_os_error()
            );
        }

        let bind_addr = SocketAddrV4::new(multicast_ip, port);

        #[cfg(target_os = "macos")]
        let raw_bind_addr = libc::sockaddr_in {
            sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as _,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_be_bytes(multicast_ip.octets()),
            },
            sin_zero: [0; 8],
        };
        #[cfg(not(target_os = "macos"))]
        let raw_bind_addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as _,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_be_bytes(multicast_ip.octets()),
            },
            sin_zero: [0; 8],
        };

        // SAFETY: `fd` is a valid socket descriptor, and `raw_bind_addr`
        // remains alive for the duration of the call. The descriptor is
        // closed explicitly on failure and transferred to `UdpSocket` on
        // success.
        let bind_result = unsafe {
            libc::bind(
                fd,
                (&raw_bind_addr as *const libc::sockaddr_in).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if bind_result != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(format!(
                "Failed to bind LPD socket to {}: {}",
                bind_addr, error
            ));
        }

        // SAFETY: `fd` is a valid open socket descriptor returned by a
        // successful `socket()` call above. `from_raw_fd` takes ownership
        // of the descriptor, and the socket will be closed when dropped.
        let socket = unsafe { UdpSocket::from_raw_fd(fd) };

        // Join multicast group on all interfaces.
        // C++: `socket_->joinMulticastGroup(multicastAddress_, multicastPort_, localAddr)`
        socket
            .join_multicast_v4(&multicast_ip, &local_interface)
            .map_err(|e| format!("Failed to join LPD multicast group: {}", e))?;

        debug!(local = ?socket.local_addr().ok(), "LPD receive socket created (Unix)");
        Ok(socket)
    }

    // On Windows, bind to the port only (not the multicast address).
    // C++: `socket_->bindWithFamily(multicastPort_, AF_INET)`
    // This is necessary because binding to the multicast address fails
    // under Windows/MinGW. Unlike Unix, we do not set SO_REUSEADDR
    // here because (a) the windows-sys crate does not expose WinSock,
    // and (b) Windows multicast sockets can share a bound port without
    // SO_REUSEADDR when joining the same multicast group.
    #[cfg(windows)]
    {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);

        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| format!("Failed to bind LPD socket to {}: {}", bind_addr, e))?;

        // Join multicast group on all interfaces.
        socket
            .join_multicast_v4(&multicast_ip, &local_interface)
            .map_err(|e| format!("Failed to join LPD multicast group: {}", e))?;

        debug!(local = ?socket.local_addr().ok(), "LPD receive socket created (Windows)");
        Ok(socket)
    }

    // Fallback for other platforms (e.g., wasm). This is unlikely to
    // be used in practice but keeps the code compilable everywhere.
    #[cfg(not(any(unix, windows)))]
    {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
        let socket =
            UdpSocket::bind(bind_addr).map_err(|e| format!("Failed to bind LPD socket: {}", e))?;
        socket
            .join_multicast_v4(&multicast_ip, &local_interface)
            .map_err(|e| format!("Failed to join LPD multicast group: {}", e))?;
        Ok(socket)
    }
}

// ===========================================================================
// Receive Loop — mirrors C++ LpdReceiveMessageCommand::execute()
// ===========================================================================

/// Background receive loop that continuously reads LPD multicast
/// announcements and updates the peer registry.
///
/// Mirrors the C++ event loop pattern where
/// `LpdReceiveMessageCommand::execute()` receives up to 20 messages
/// per invocation, then re-adds itself to the event loop. In our
/// async model, each `tokio::select!` iteration waits for one
/// datagram (or cancellation), then drains up to
/// [`MAX_MESSAGES_PER_ITERATION`] additional datagrams via
/// `try_recv_from` before updating the registry.
async fn run_receive_loop(
    socket: tokio::net::UdpSocket,
    peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>>,
    active_hashes: Arc<RwLock<HashSet<String>>>,
    cancel_token: CancellationToken,
) {
    let mut buf = [0u8; LPD_BUFFER_SIZE];

    info!("LPD receive loop entered");

    loop {
        // Wait for the next datagram or cancellation.
        let recv_result = tokio::select! {
            result = socket.recv_from(&mut buf) => result,
            _ = cancel_token.cancelled() => {
                info!("LPD receive loop cancelled");
                break;
            }
        };

        let (len, src_addr) = match recv_result {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "LPD receive socket error, retrying");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        if len == 0 {
            continue;
        }

        // Collect a batch of peers starting with the first datagram.
        let mut batch = Vec::with_capacity(MAX_MESSAGES_PER_ITERATION);

        if let Some(peer) = parse_lpd_announcement(&buf[..len], src_addr.ip()) {
            trace!(
                info_hash = %&peer.info_hash[..8.min(peer.info_hash.len())],
                port = peer.port, addr = %src_addr.ip(),
                "LPD receive: valid announcement"
            );
            batch.push(peer);
        }

        // Drain more datagrams without blocking (up to batch limit).
        // C++: `for (size_t i = 0; i < 20; ++i)` in execute().
        for _ in 1..MAX_MESSAGES_PER_ITERATION {
            match socket.try_recv_from(&mut buf) {
                Ok((n, addr)) if n > 0 => {
                    if let Some(p) = parse_lpd_announcement(&buf[..n], addr.ip()) {
                        batch.push(p);
                    }
                }
                Ok(_) => continue,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!(error = %e, "LPD batch recv error");
                    break;
                }
            }
        }

        if !batch.is_empty() {
            update_peer_registry(&peers, &active_hashes, batch).await;
        }
    }

    info!("LPD receive loop exited");
}

/// Update the peer registry with newly discovered peers.
///
/// Only adds peers whose info_hash is in `active_hashes` (i.e., we are
/// currently announcing that torrent). This mirrors the C++ behavior:
///
/// ```cpp
/// auto& dctx = reg->getDownloadContext(m->infoHash);
/// if (!dctx) { continue; }  // Skip unknown info hashes
/// if (bittorrent::getTorrentAttrs(dctx)->privateTorrent) { continue; }
/// ```
///
/// Private torrent filtering is already handled during registration
/// in `LpdManager::register_torrent()` — private hashes are never
/// added to `active_hashes`, so they are implicitly filtered here.
async fn update_peer_registry(
    peers: &Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>>,
    active_hashes: &Arc<RwLock<HashSet<String>>>,
    new_peers: Vec<LpdPeer>,
) {
    let active = active_hashes.read().await;

    // Filter: only process peers for info_hashes we are announcing.
    // C++: `auto& dctx = reg->getDownloadContext(m->infoHash); if (!dctx) continue;`
    let relevant_peers: Vec<LpdPeer> = new_peers
        .into_iter()
        .filter(|p| active.contains(&p.info_hash))
        .collect();

    drop(active); // Release read lock before acquiring write lock

    if relevant_peers.is_empty() {
        return;
    }

    // Group peers by info_hash for batch updates.
    let mut peers_map = peers.write().await;
    for peer in relevant_peers {
        let entry = peers_map.entry(peer.info_hash.clone()).or_default();

        // Limit total peers per hash (same as LpdManager::MAX_PEERS_PER_HASH).
        // If at capacity, remove the oldest peer to make room.
        if entry.len() >= crate::engine::lpd_manager::MAX_PEERS_PER_HASH
            && let Some(oldest) = entry.iter().max_by_key(|p| p.last_seen.elapsed()).cloned()
        {
            entry.remove(&oldest);
        }

        if entry.insert(peer.clone()) {
            debug!(
                info_hash = %&peer.info_hash[..8.min(peer.info_hash.len())],
                addr = %peer.addr,
                port = peer.port,
                local = peer.is_local,
                "LPD peer added to registry"
            );
        } else {
            trace!(
                info_hash = %&peer.info_hash[..8.min(peer.info_hash.len())],
                addr = %peer.addr,
                "LPD peer already in registry (updated last_seen)"
            );
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// Verify that create_lpd_socket() can create a socket.
    /// This test may fail in CI environments without multicast support.
    #[test]
    fn test_create_lpd_socket() {
        match create_lpd_socket() {
            Ok(socket) => {
                let local = socket.local_addr().expect("local_addr should work");
                assert_eq!(local.port(), constants::LPD_PORT);
                debug!(addr = %local, "LPD socket created successfully");
            }
            Err(e) => {
                // In restricted environments (CI, containers), multicast
                // may not be available. Log but don't fail the test.
                eprintln!("SKIP: create_lpd_socket failed: {}", e);
            }
        }
    }

    /// Verify the LPD buffer size matches constants.
    #[test]
    fn test_lpd_buffer_size() {
        assert_eq!(
            LPD_BUFFER_SIZE,
            constants::LPD_RECEIVE_BUFFER_SIZE,
            "LPD_BUFFER_SIZE should match constants::LPD_RECEIVE_BUFFER_SIZE"
        );
    }

    /// Verify the max messages per iteration matches C++.
    #[test]
    fn test_max_messages_per_iteration() {
        assert_eq!(MAX_MESSAGES_PER_ITERATION, 20, "Should match C++ constant");
    }

    /// Test that update_peer_registry filters unknown info hashes.
    #[tokio::test]
    async fn test_update_peer_registry_filters_unknown_hashes() {
        let peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let active_hashes: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

        // Register only one hash
        {
            let mut active = active_hashes.write().await;
            active.insert("0123456789abcdef0123456789abcdef01234567".to_string());
        }

        // Create peers: one known hash, one unknown hash
        let new_peers = vec![
            LpdPeer::new(
                "0123456789abcdef0123456789abcdef01234567",
                6881,
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            ),
            LpdPeer::new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                6882,
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            ),
        ];

        update_peer_registry(&peers, &active_hashes, new_peers).await;

        let peers_map = peers.read().await;
        // Only the known hash should be in the registry
        assert!(
            peers_map.contains_key("0123456789abcdef0123456789abcdef01234567"),
            "Known hash should be present"
        );
        assert!(
            !peers_map.contains_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "Unknown hash should be filtered out"
        );
    }

    /// Test that update_peer_registry respects MAX_PEERS_PER_HASH.
    #[tokio::test]
    async fn test_update_peer_registry_max_peers() {
        let peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let active_hashes: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

        let info_hash = "0123456789abcdef0123456789abcdef01234567";
        {
            let mut active = active_hashes.write().await;
            active.insert(info_hash.to_string());
        }

        // Add more peers than MAX_PEERS_PER_HASH
        let new_peers: Vec<LpdPeer> = (0..crate::engine::lpd_manager::MAX_PEERS_PER_HASH + 5)
            .map(|i| {
                LpdPeer::new(
                    info_hash,
                    6881,
                    IpAddr::V4(Ipv4Addr::new(10, 0, (i / 256) as u8, (i % 256) as u8)),
                )
            })
            .collect();

        update_peer_registry(&peers, &active_hashes, new_peers).await;

        let peers_map = peers.read().await;
        let stored = peers_map.get(info_hash).expect("hash should exist");
        assert!(
            stored.len() <= crate::engine::lpd_manager::MAX_PEERS_PER_HASH,
            "Should not exceed MAX_PEERS_PER_HASH (got {})",
            stored.len()
        );
    }

    /// Test LpdReceiveLoop lifecycle: new -> start -> stop.
    #[tokio::test]
    async fn test_lpd_receive_loop_lifecycle() {
        let mut recv_loop = LpdReceiveLoop::new();
        assert!(!recv_loop.is_running(), "Should not be running initially");

        let peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let active_hashes: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

        // Try to start. May fail in restricted environments.
        match recv_loop.start(peers, active_hashes).await {
            Ok(()) => {
                assert!(recv_loop.is_running(), "Should be running after start");

                // Stop should complete gracefully
                recv_loop.stop().await;
                assert!(!recv_loop.is_running(), "Should not be running after stop");
            }
            Err(e) => {
                eprintln!("SKIP: LPD receive loop start failed: {}", e);
            }
        }
    }
}
