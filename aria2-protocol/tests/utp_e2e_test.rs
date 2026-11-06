#![cfg(feature = "bittorrent")]

//! Public uTP library contracts and packet fixtures.
//! Wire interoperability against an independent UDP peer is covered separately
//! in `utp_wire_interop_test.rs`; packet-only scenarios do not validate a runtime.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::utp::congestion::{
    LEDBAT_MAX_CWND, LEDBAT_MIN_CWND, LEDBAT_TARGET_DELAY, LedbatController,
};
use aria2_protocol::bittorrent::utp::connection::{ConnectionState, UtpConnection};
use aria2_protocol::bittorrent::utp::metrics::{DelayEstimator, RttEstimator};
use aria2_protocol::bittorrent::utp::packet::{PacketType, UtpPacket};
use aria2_protocol::bittorrent::utp::socket::UtpSocket;

// ===========================================================================
// Helper functions for test setup
// ===========================================================================

/// Create a mock UDP pair for testing
fn create_udp_pair() -> (UdpSocket, UdpSocket) {
    let server = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind server socket");
    let client = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind client socket");

    server
        .set_nonblocking(true)
        .expect("Failed to set server nonblocking");
    client
        .set_nonblocking(true)
        .expect("Failed to set client nonblocking");

    (server, client)
}

/// Get local address of a socket
fn get_addr(socket: &UdpSocket) -> std::net::SocketAddr {
    socket.local_addr().expect("Failed to get local address")
}

/// Wait for packet with timeout using polling.
///
/// Uses non-blocking `recv_from` with a busy-poll loop instead of
/// `set_read_timeout` on a blocking socket. This is more portable:
/// macOS `set_read_timeout` + non-blocking mode is unreliable, and
/// changing the blocking mode momentarily introduces races.
fn recv_with_timeout(
    socket: &UdpSocket,
    timeout_ms: u64,
) -> Option<(Vec<u8>, std::net::SocketAddr)> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let mut buf = vec![0u8; 65535];

    while start.elapsed() < timeout {
        match socket.recv_from(&mut buf) {
            Ok((len, addr)) => return Some((buf[..len].to_vec(), addr)),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return None,
        }
    }
    None
}

/// Send raw packet to address
fn send_raw(socket: &UdpSocket, data: &[u8], addr: std::net::SocketAddr) -> bool {
    socket.send_to(data, addr).is_ok()
}

#[path = "utp_e2e_test/connection.rs"]
mod connection;
#[path = "utp_e2e_test/metrics.rs"]
mod metrics;
#[path = "utp_e2e_test/packet.rs"]
mod packet;
#[path = "utp_e2e_test/udp.rs"]
mod udp;
