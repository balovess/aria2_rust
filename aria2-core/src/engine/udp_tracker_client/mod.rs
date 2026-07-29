mod connection;
mod protocol;
mod scrape;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tracing::info;

pub(crate) use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent;

pub(crate) use protocol::UdpTrackerRequest;

pub use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::AnnounceResponse;

pub(crate) const REQUEST_TIMEOUT_SECS: u64 = 15;
pub(crate) const MAX_RETRIES: u32 = 3;

pub(crate) struct ConnectionState {
    pub(crate) id: u64,
    pub(crate) updated_at: Instant,
}

pub struct UdpTrackerClient {
    pub(crate) socket: Arc<tokio::net::UdpSocket>,
    pub(crate) conn_cache: HashMap<SocketAddr, ConnectionState>,
    pub(crate) pending: VecDeque<UdpTrackerRequest>,
    pub(crate) inflight: VecDeque<UdpTrackerRequest>,
    pub(crate) waiting_for_conn: VecDeque<UdpTrackerRequest>,
    pub(crate) txn_map: HashMap<u32, usize>,
    next_txn_id: u32,
}

impl UdpTrackerClient {
    pub async fn new(bind_port: u16) -> Result<Self, String> {
        let addr = format!("0.0.0.0:{}", bind_port);
        let socket = tokio::net::UdpSocket::bind(&addr)
            .await
            .map_err(|e| format!("UDP bind failed on {}: {}", addr, e))?;

        info!("UdpTrackerClient bound to {}", addr);

        Ok(Self {
            socket: Arc::new(socket),
            conn_cache: HashMap::new(),
            pending: VecDeque::new(),
            inflight: VecDeque::new(),
            waiting_for_conn: VecDeque::new(),
            txn_map: HashMap::new(),
            next_txn_id: Self::initial_txn_id(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_announce(
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

    pub fn no_pending(&self) -> bool {
        self.pending.is_empty() && self.inflight.is_empty() && self.waiting_for_conn.is_empty()
    }

    pub fn completed_requests(&self) -> Vec<&AnnounceResponse> {
        self.pending
            .iter()
            .filter_map(|r| r.reply.as_ref())
            .collect()
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

pub type SharedUdpClient = Arc<Mutex<UdpTrackerClient>>;

impl UdpTrackerClient {
    pub async fn create_shared(bind_port: u16) -> Result<SharedUdpClient, String> {
        let client = Self::new(bind_port).await?;
        Ok(Arc::new(Mutex::new(client)))
    }
}
