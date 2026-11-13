//! BEP 15 UDP tracker transaction client over a caller-bound socket.

mod connection;
mod request;
mod scrape;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

pub use crate::bittorrent::tracker::udp_tracker_protocol::{AnnounceResponse, UdpEvent};

pub(crate) use request::UdpTrackerRequest;
pub use request::{ScrapeResult, UdpAnnounceParams, UdpError};

pub(crate) const MAX_RETRIES: u32 = 2;
const INITIAL_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const RETRY_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) fn response_timeout(fail_count: u32) -> std::time::Duration {
    if fail_count == 0 {
        INITIAL_RESPONSE_TIMEOUT
    } else {
        RETRY_RESPONSE_TIMEOUT
    }
}

pub(crate) struct ConnectionState {
    pub(crate) id: u64,
    pub(crate) updated_at: Instant,
}

pub struct UdpTrackerClient {
    pub(crate) socket: tokio::net::UdpSocket,
    pub(crate) conn_cache: HashMap<SocketAddr, ConnectionState>,
    pub(crate) pending: VecDeque<UdpTrackerRequest>,
    pub(crate) inflight: VecDeque<UdpTrackerRequest>,
    pub(crate) waiting_for_conn: VecDeque<UdpTrackerRequest>,
    pub(crate) txn_map: HashMap<u32, usize>,
}

impl UdpTrackerClient {
    /// Create a tracker protocol client over a socket bound by the caller.
    ///
    /// The caller selects the local address, address family, and outbound
    /// network policy before transferring the socket to this protocol client.
    pub fn new(socket: tokio::net::UdpSocket) -> Self {
        Self {
            socket,
            conn_cache: HashMap::new(),
            pending: VecDeque::new(),
            inflight: VecDeque::new(),
            waiting_for_conn: VecDeque::new(),
            txn_map: HashMap::new(),
        }
    }

    /// Return the local endpoint of the caller-bound UDP socket.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_announce(
        &mut self,
        addr: &SocketAddr,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: i64,
        left: i64,
        uploaded: i64,
        event: UdpEvent,
        num_want: i32,
        port: u16,
    ) {
        let req = UdpTrackerRequest::new(
            *addr, *info_hash, *peer_id, downloaded, left, uploaded, event, num_want, port,
        );
        self.pending.push_back(req);
        tracing::debug!("Added announce request for {}", addr);
    }

    pub(crate) fn next_txn(&mut self) -> u32 {
        loop {
            let id = rand::random::<u32>();
            if !self.txn_map.contains_key(&id) {
                return id;
            }
        }
    }
}
