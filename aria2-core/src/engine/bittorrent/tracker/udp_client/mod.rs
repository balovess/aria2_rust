mod connection;
mod request;
mod scrape;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use tracing::info;

use crate::network::OutboundNetworkPolicy;

pub(crate) use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent;

pub(crate) use request::UdpTrackerRequest;
pub use request::{UdpAnnounceParams, UdpError};

pub use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::AnnounceResponse;

pub(crate) const MAX_RETRIES: u32 = 3;

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
    next_txn_id: u32,
}

impl UdpTrackerClient {
    pub async fn new_with_policy(
        bind_port: u16,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self, String> {
        let socket = policy
            .bind_udp(bind_port)
            .await
            .map_err(|e| format!("UDP bind failed: {e}"))?;
        let addr = socket
            .local_addr()
            .map_err(|e| format!("UDP local address unavailable: {e}"))?;

        info!("UdpTrackerClient bound to {}", addr);

        Ok(Self {
            socket,
            conn_cache: HashMap::new(),
            pending: VecDeque::new(),
            inflight: VecDeque::new(),
            waiting_for_conn: VecDeque::new(),
            txn_map: HashMap::new(),
            next_txn_id: Self::initial_txn_id(),
        })
    }

    pub async fn new_with_policy_for_family(
        bind_port: u16,
        policy: &OutboundNetworkPolicy,
        ipv6: bool,
    ) -> Result<Self, String> {
        let socket = policy
            .bind_udp_for_family(bind_port, ipv6)
            .await
            .map_err(|e| format!("UDP bind failed: {e}"))?;
        let addr = socket
            .local_addr()
            .map_err(|e| format!("UDP local address unavailable: {e}"))?;

        info!(%addr, ipv6, "UdpTrackerClient bound to requested address family");

        Ok(Self {
            socket,
            conn_cache: HashMap::new(),
            pending: VecDeque::new(),
            inflight: VecDeque::new(),
            waiting_for_conn: VecDeque::new(),
            txn_map: HashMap::new(),
            next_txn_id: Self::initial_txn_id(),
        })
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
        let id = self.next_txn_id;
        self.next_txn_id = id.wrapping_add(1);
        if self.next_txn_id == 0 {
            self.next_txn_id = 1;
        }
        id
    }

    fn initial_txn_id() -> u32 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let dur = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        ((dur.as_nanos() & 0xFFFFFFFF) as u32).max(1)
    }
}

const DEFAULT_UDP_TRACKER_PORT: u16 = 6881;

pub(crate) async fn resolve_udp_tracker_addr(
    url: &str,
    policy: &OutboundNetworkPolicy,
) -> Result<SocketAddr, String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| format!("invalid UDP tracker URL {url}: {error}"))?;
    if parsed.scheme() != "udp" {
        return Err(format!("invalid UDP tracker URL: {url}"));
    }
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| format!("missing UDP tracker host: {url}"))?;
    let port = parsed.port().unwrap_or(DEFAULT_UDP_TRACKER_PORT);

    policy
        .resolve_udp_host(&host, port)
        .await
        .map_err(|error| format!("failed to resolve UDP tracker {url}: {error}"))
}
