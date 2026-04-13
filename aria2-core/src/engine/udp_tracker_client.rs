use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::{
    AnnounceResponse, CONNECTION_TIMEOUT_SECS, ConnectResponse, ScrapeResult, UdpAction, UdpError,
    UdpEvent, UdpState, build_announce_request, build_connect_request, build_scrape_request,
    parse_announce_response, parse_connect_response, parse_scrape_response,
};

const REQUEST_TIMEOUT_SECS: u64 = 15;
const MAX_RETRIES: u32 = 3;

struct ConnectionState {
    id: u64,
    updated_at: Instant,
}

#[derive(Debug)]
pub struct UdpTrackerRequest {
    pub remote_addr: SocketAddr,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub downloaded: i64,
    pub left: i64,
    pub uploaded: i64,
    pub event: UdpEvent,
    pub num_want: i32,
    pub port: u16,
    pub state: UdpState,
    pub error: Option<UdpError>,
    pub dispatched_at: Option<Instant>,
    pub fail_count: u32,
    pub reply: Option<AnnounceResponse>,
    /// Scrape results populated when this is a scrape request
    pub scrape_results: Option<Vec<ScrapeResult>>,
    /// Info hashes for scrape requests (can be multiple)
    pub scrape_info_hashes: Vec<[u8; 20]>,
    txn_id: u32,
}

impl UdpTrackerRequest {
    fn new(
        addr: SocketAddr,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
        downloaded: i64,
        left: i64,
        uploaded: i64,
        event: UdpEvent,
        num_want: i32,
        port: u16,
    ) -> Self {
        Self {
            remote_addr: addr,
            info_hash,
            peer_id,
            downloaded,
            left,
            uploaded,
            event,
            num_want,
            port,
            state: UdpState::Pending,
            error: None,
            dispatched_at: None,
            fail_count: 0,
            reply: None,
            scrape_results: None,
            scrape_info_hashes: Vec::new(),
            txn_id: 0,
        }
    }
}

pub struct UdpTrackerClient {
    socket: Arc<tokio::net::UdpSocket>,
    conn_cache: HashMap<SocketAddr, ConnectionState>,
    pending: VecDeque<UdpTrackerRequest>,
    inflight: VecDeque<UdpTrackerRequest>,
    waiting_for_conn: VecDeque<UdpTrackerRequest>,
    txn_map: HashMap<u32, usize>,
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
        debug!("Added announce request for {}", addr);
    }

    /// Add a SCRAPE request to query statistics for one or more info hashes
    ///
    /// # Arguments
    /// * `addr` - UDP tracker socket address
    /// * `info_hashes` - Slice of 20-byte info hashes to query (max ~74 per request)
    pub async fn add_scrape(&mut self, addr: &SocketAddr, info_hashes: &[[u8; 20]]) {
        // Use first info hash for the request struct (scrape can have multiple)
        let first_ih = if info_hashes.is_empty() {
            [0u8; 20]
        } else {
            info_hashes[0]
        };

        let mut req =
            UdpTrackerRequest::new(*addr, first_ih, [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
        req.scrape_info_hashes = info_hashes.to_vec();
        self.pending.push_back(req);
        debug!(
            "Added scrape request for {} ({} hashes)",
            addr,
            info_hashes.len()
        );
    }

    pub async fn process_one(&mut self) -> bool {
        loop {
            if self.pending.is_empty() && self.waiting_for_conn.is_empty() {
                return false;
            }

            if let Some(mut req) = self.pending.pop_front() {
                let host_key = req.remote_addr;

                if let Some(conn) = self.conn_cache.get(&host_key) {
                    if conn.updated_at.elapsed().as_secs() < CONNECTION_TIMEOUT_SECS {
                        // Route to appropriate send method based on request type
                        if !req.scrape_info_hashes.is_empty() {
                            return self.send_scrape(&mut req, conn.id).await;
                        }
                        return self.send_announce(&mut req, conn.id).await;
                    } else {
                        self.conn_cache.remove(&host_key);
                        debug!("Connection cache expired for {}", host_key);
                    }
                }

                if !self.is_connecting_to(&host_key) {
                    self.waiting_for_conn.push_back(req);
                    return self.send_connect(host_key).await;
                }

                self.waiting_for_conn.push_back(req);
                debug!("Waiting for connection to {}", host_key);
            } else if let Some(req) = self.waiting_for_conn.pop_front() {
                self.pending.push_front(req);
            } else {
                return false;
            }
        }
    }

    async fn send_announce(&mut self, req: &mut UdpTrackerRequest, conn_id: u64) -> bool {
        let txn_id = self.next_txn();
        req.txn_id = txn_id;
        req.dispatched_at = Some(Instant::now());
        req.state = UdpState::Pending;

        let payload = build_announce_request(
            conn_id,
            txn_id,
            &req.info_hash,
            &req.peer_id,
            req.downloaded,
            req.left,
            req.uploaded,
            req.event,
            0,
            0,
            req.num_want,
            req.port,
        );

        match self.socket.send_to(&payload, req.remote_addr).await {
            Ok(len) => {
                self.txn_map.insert(txn_id, self.inflight.len());
                self.inflight.push_back(std::mem::replace(
                    req,
                    UdpTrackerRequest::new(
                        req.remote_addr,
                        req.info_hash,
                        req.peer_id,
                        req.downloaded,
                        req.left,
                        req.uploaded,
                        req.event,
                        req.num_want,
                        req.port,
                    ),
                ));
                debug!(
                    "Sent ANNOUNCE {} bytes to {} (txn={})",
                    len, req.remote_addr, txn_id
                );
                true
            }
            Err(e) => {
                warn!("Send ANNOUNCE to {} failed: {}", req.remote_addr, e);
                req.fail_count += 1;
                req.error = Some(UdpError::Network);
                if req.fail_count < MAX_RETRIES {
                    self.pending.push_front(std::mem::replace(
                        req,
                        UdpTrackerRequest::new(
                            req.remote_addr,
                            req.info_hash,
                            req.peer_id,
                            req.downloaded,
                            req.left,
                            req.uploaded,
                            req.event,
                            req.num_want,
                            req.port,
                        ),
                    ));
                }
                true
            }
        }
    }

    async fn send_scrape(&mut self, req: &mut UdpTrackerRequest, conn_id: u64) -> bool {
        let txn_id = self.next_txn();
        req.txn_id = txn_id;
        req.dispatched_at = Some(Instant::now());
        req.state = UdpState::Pending;

        // Build scrape payload with all info hashes from this request
        let hashes: Vec<[u8; 20]> = req.scrape_info_hashes.clone();
        if hashes.is_empty() {
            warn!("Scrape request with no info hashes for {}", req.remote_addr);
            return true;
        }

        let payload = build_scrape_request(conn_id, txn_id, &hashes);

        match self.socket.send_to(&payload, req.remote_addr).await {
            Ok(len) => {
                self.txn_map.insert(txn_id, self.inflight.len());
                // Preserve scrape_info_hashes when replacing the request
                let mut replacement = UdpTrackerRequest::new(
                    req.remote_addr,
                    req.info_hash,
                    req.peer_id,
                    req.downloaded,
                    req.left,
                    req.uploaded,
                    req.event,
                    req.num_want,
                    req.port,
                );
                replacement.scrape_info_hashes = std::mem::take(&mut req.scrape_info_hashes);
                self.inflight.push_back(std::mem::replace(req, replacement));
                debug!(
                    "Sent SCRAPE {} bytes to {} (txn={}, {} hashes)",
                    len,
                    req.remote_addr,
                    txn_id,
                    hashes.len()
                );
                true
            }
            Err(e) => {
                warn!("Send SCRAPE to {} failed: {}", req.remote_addr, e);
                req.fail_count += 1;
                req.error = Some(UdpError::Network);
                if req.fail_count < MAX_RETRIES {
                    self.pending.push_front(std::mem::replace(
                        req,
                        UdpTrackerRequest::new(
                            req.remote_addr,
                            req.info_hash,
                            req.peer_id,
                            req.downloaded,
                            req.left,
                            req.uploaded,
                            req.event,
                            req.num_want,
                            req.port,
                        ),
                    ));
                }
                true
            }
        }
    }

    async fn send_connect(&mut self, addr: SocketAddr) -> bool {
        let txn_id = self.next_txn();

        let payload = build_connect_request(txn_id);

        match self.socket.send_to(&payload, addr).await {
            Ok(len) => {
                let mut dummy_req = UdpTrackerRequest::new(
                    addr,
                    [0u8; 20],
                    [0u8; 20],
                    0,
                    0,
                    0,
                    UdpEvent::None,
                    0,
                    0,
                );
                dummy_req.txn_id = txn_id;
                dummy_req.dispatched_at = Some(Instant::now());
                dummy_req.state = UdpState::Pending;
                self.txn_map.insert(txn_id, self.inflight.len());
                self.inflight.push_back(dummy_req);
                debug!("Sent CONNECT {} bytes to {} (txn={})", len, addr, txn_id);
                true
            }
            Err(e) => {
                warn!("Send CONNECT to {} failed: {}", addr, e);
                true
            }
        }
    }

    pub async fn handle_response(&mut self, data: &[u8], from: &SocketAddr) {
        if data.len() < 4 {
            warn!("Short response from {}: {} bytes", from, data.len());
            return;
        }

        let action_val = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let txn_id = if data.len() >= 8 {
            u32::from_be_bytes([data[4], data[5], data[6], data[7]])
        } else {
            warn!("Response too short for txn_id from {}", from);
            return;
        };

        let idx = match self.txn_map.remove(&txn_id) {
            Some(i) => i,
            None => {
                debug!("Unknown txn_id {} from {}", txn_id, from);
                return;
            }
        };

        if idx >= self.inflight.len() {
            warn!(
                "Invalid index {} for txn_id {} (inflight={})",
                idx,
                txn_id,
                self.inflight.len()
            );
            return;
        }

        let mut req = self.inflight.remove(idx).unwrap_or_else(|| {
            UdpTrackerRequest::new(*from, [0u8; 20], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0)
        });

        match UdpAction::from_i32(action_val) {
            Some(UdpAction::Connect) => match parse_connect_response(data) {
                Ok(ConnectResponse { connection_id, .. }) => {
                    info!(
                        "CONNECT response from {}, conn_id=0x{:016X}",
                        from, connection_id
                    );
                    self.conn_cache.insert(
                        *from,
                        ConnectionState {
                            id: connection_id,
                            updated_at: Instant::now(),
                        },
                    );

                    while let Some(waiting) = self.waiting_for_conn.pop_front() {
                        self.pending.push_front(waiting);
                    }
                }
                Err(e) => {
                    warn!("Parse CONNECT response from {} failed: {}", from, e);
                    req.error = Some(UdpError::TrackerError);
                }
            },
            Some(UdpAction::Announce) => {
                match parse_announce_response(data) {
                    Ok(resp) => {
                        info!(
                            "ANNOUNCE response from {}: {} peers, interval={}s",
                            from,
                            resp.peers.len(),
                            resp.interval
                        );
                        req.reply = Some(resp);
                        req.state = UdpState::Complete;
                        req.error = Some(UdpError::Success);
                    }
                    Err(e) => {
                        warn!("Parse ANNOUNCE response from {} failed: {}", from, e);
                        req.error = Some(UdpError::TrackerError);
                    }
                }
                self.pending.push_back(req);
            }
            Some(UdpAction::Error) => {
                let msg_len = (data.len() - 8).min(256);
                let msg = String::from_utf8_lossy(&data[8..8 + msg_len]);
                warn!("Tracker error from {}: {}", from, msg);
                req.error = Some(UdpError::TrackerError);
                self.pending.push_back(req);
            }
            Some(UdpAction::Scrape) => match parse_scrape_response(data) {
                Ok(results) => {
                    info!(
                        "SCRAPE response from {}: {} info hashes scraped",
                        from,
                        results.len()
                    );
                    // Store scrape results in a dedicated collection
                    for result in &results {
                        debug!(
                            "  seeders={} leechers={} completed={}",
                            result.seeders, result.leechers, result.completed
                        );
                    }
                    req.scrape_results = Some(results);
                    req.state = UdpState::Complete;
                    req.error = Some(UdpError::Success);
                    self.pending.push_back(req);
                }
                Err(e) => {
                    warn!("Parse SCRAPE response from {} failed: {}", from, e);
                    req.error = Some(UdpError::TrackerError);
                    self.pending.push_back(req);
                }
            },
            _ => {
                warn!("Unknown action {} from {}", action_val, from);
                req.error = Some(UdpError::TrackerError);
                self.pending.push_back(req);
            }
        }
    }

    pub async fn handle_timeouts(&mut self) {
        let now = Instant::now();
        let expired: Vec<usize> = self
            .inflight
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.dispatched_at.is_some_and(|t| {
                    t.duration_since(now) > Duration::from_secs(REQUEST_TIMEOUT_SECS)
                })
            })
            .map(|(i, _)| i)
            .collect();

        for idx in expired.into_iter().rev() {
            if idx < self.inflight.len() {
                let mut req = self.inflight.remove(idx).unwrap_or_else(|| {
                    UdpTrackerRequest::new(
                        std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                            std::net::Ipv4Addr::UNSPECIFIED,
                            0,
                        )),
                        [0u8; 20],
                        [0u8; 20],
                        0,
                        0,
                        0,
                        UdpEvent::None,
                        0,
                        0,
                    )
                });
                if req.txn_id != 0 {
                    self.txn_map.remove(&req.txn_id);
                }
                req.fail_count += 1;
                if req.fail_count < MAX_RETRIES {
                    debug!(
                        "Timeout retry {}/{} for txn_id={}",
                        req.fail_count, MAX_RETRIES, req.txn_id
                    );
                    req.dispatched_at = None;
                    self.pending.push_back(req);
                } else {
                    warn!("Max retries exceeded for txn_id={}", req.txn_id);
                    req.error = Some(UdpError::Timeout);
                }
            }
        }

        let stale_addrs: Vec<SocketAddr> = self
            .conn_cache
            .iter()
            .filter(|(_, s)| s.updated_at.elapsed().as_secs() > CONNECTION_TIMEOUT_SECS)
            .map(|(&a, _)| a)
            .collect();

        for addr in stale_addrs {
            self.conn_cache.remove(&addr);
            debug!("Removed stale connection cache for {}", addr);
        }
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

    /// Get all completed scrape results from pending requests
    pub fn completed_scrape_results(&self) -> Vec<&Vec<ScrapeResult>> {
        self.pending
            .iter()
            .filter_map(|r| r.scrape_results.as_ref())
            .collect()
    }

    pub fn socket(&self) -> Arc<tokio::net::UdpSocket> {
        Arc::clone(&self.socket)
    }

    fn is_connecting_to(&self, addr: &SocketAddr) -> bool {
        self.inflight
            .iter()
            .any(|r| &r.remote_addr == addr && r.reply.is_none())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_client_creation() {
        let client = UdpTrackerClient::new(0).await;
        assert!(
            client.is_ok(),
            "UDP client creation should succeed with port 0"
        );
        let c = client.unwrap();
        assert!(c.no_pending());
        assert!(c.completed_requests().is_empty());
    }

    #[tokio::test]
    async fn test_add_announce_request() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let ih = [0xABu8; 20];
        let pid = [0xCDu8; 20];

        client
            .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
            .await;
        assert_eq!(client.pending.len(), 1);

        client
            .add_announce(&addr, &ih, &pid, 500, 500, 0, UdpEvent::None, -1, 6881)
            .await;
        assert_eq!(client.pending.len(), 2);
    }

    #[tokio::test]
    async fn test_process_one_needs_connection() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let ih = [0x12u8; 20];
        let pid = [0x34u8; 20];

        client
            .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
            .await;
        let processed = client.process_one().await;
        assert!(processed, "Should have processed the connect step");
        assert!(
            !client.inflight.is_empty(),
            "Should have an in-flight CONNECT"
        );
    }

    #[tokio::test]
    async fn test_no_pending_returns_false_when_empty() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        assert!(client.no_pending());
        let processed = client.process_one().await;
        assert!(!processed, "process_one should return false when empty");
    }

    #[tokio::test]
    async fn test_handle_connect_response() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let ih = [0x11u8; 20];
        let pid = [0x22u8; 20];

        client
            .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
            .await;
        client.process_one().await;

        let mut resp_data = vec![0u8; 16];
        resp_data[0..4].copy_from_slice(&0i32.to_be_bytes());
        let txn_id = client.inflight.front().map(|r| r.txn_id).unwrap_or(0);
        resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
        resp_data[8..16].copy_from_slice(&0x123456789ABCDEF0u64.to_be_bytes());

        client.handle_response(&resp_data, &addr).await;
        assert!(
            client.conn_cache.contains_key(&addr),
            "Should cache connection after CONNECT response"
        );
    }

    #[tokio::test]
    async fn test_handle_announce_response_with_peers() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();

        let ih = [0x33u8; 20];
        let pid = [0x44u8; 20];
        let txn_id = client.next_txn();

        client.txn_map.insert(txn_id, 0);
        let mut dummy_req =
            UdpTrackerRequest::new(addr, ih, pid, 0, 1000, 0, UdpEvent::Started, 50, 6881);
        dummy_req.txn_id = txn_id;
        dummy_req.dispatched_at = Some(Instant::now());
        client.inflight.push_back(dummy_req);

        let mut resp_data = vec![0u8; 26];
        resp_data[0..4].copy_from_slice(&1i32.to_be_bytes());
        resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
        resp_data[8..12].copy_from_slice(&900u32.to_be_bytes());
        resp_data[12..16].copy_from_slice(&5u32.to_be_bytes());
        resp_data[16..20].copy_from_slice(&3u32.to_be_bytes());
        resp_data.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0x04, 192, 168, 1, 100, 0x1F, 0x90]);

        client.handle_response(&resp_data, &addr).await;

        let completed = client.completed_requests();
        assert!(
            !completed.is_empty(),
            "Should have at least one completed announce"
        );
        assert!(
            completed[0].peers.len() >= 2,
            "Should have at least 2 peers"
        );
        assert_eq!(completed[0].interval, 900);
        assert_eq!(completed[0].leechers, 5);
        assert_eq!(completed[0].seeders, 3);
    }

    #[tokio::test]
    async fn test_handle_error_response() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let ih = [0x55u8; 20];
        let pid = [0x66u8; 20];

        client
            .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
            .await;
        client.process_one().await;

        let txn_id = client.inflight.front().map(|r| r.txn_id).unwrap_or(0);
        let mut err_data = vec![0u8; 23];
        err_data[0..4].copy_from_slice(&3i32.to_be_bytes());
        err_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
        err_data[8..23].copy_from_slice(b"tracker offline");

        client.handle_response(&err_data, &addr).await;
        assert!(
            !client.conn_cache.contains_key(&addr),
            "Error should not create cache entry"
        );
    }

    #[tokio::test]
    async fn test_timeout_cleaning() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let ih = [0x77u8; 20];
        let pid = [0x88u8; 20];

        client
            .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
            .await;
        client.process_one().await;
        assert_eq!(client.inflight.len(), 1);

        tokio::time::sleep(Duration::from_millis(100)).await;
        client.handle_timeouts().await;
        assert_eq!(client.inflight.len(), 1, "Not yet timed out");

        for req in &mut client.inflight {
            req.dispatched_at =
                Some(Instant::now() - Duration::from_secs(REQUEST_TIMEOUT_SECS + 1));
        }
        client.handle_timeouts().await;
        assert!(
            client.inflight.is_empty()
                || client.pending.len() > 0
                || !client.waiting_for_conn.is_empty(),
            "Timed-out request should be moved"
        );
    }

    #[tokio::test]
    async fn test_txn_id_generation() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let mut txn_ids = Vec::new();
        for _ in 0..5 {
            txn_ids.push(client.next_txn());
            client.next_txn();
        }
        let unique: std::collections::HashSet<_> = txn_ids.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            txn_ids.len(),
            "Transaction IDs should all be unique"
        );
    }

    #[tokio::test]
    async fn test_shared_client_creation() {
        let shared = UdpTrackerClient::create_shared(0).await;
        assert!(shared.is_ok(), "Shared client creation should succeed");
    }

    // --- Scrape tests ---

    #[tokio::test]
    async fn test_add_scrape_request() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let hashes = [[0xAAu8; 20], [0xBBu8; 20], [0xCCu8; 20]];

        client.add_scrape(&addr, &hashes).await;
        assert_eq!(client.pending.len(), 1);

        let req = &client.pending[0];
        assert!(!req.scrape_info_hashes.is_empty());
        assert_eq!(req.scrape_info_hashes.len(), 3);
        assert_eq!(req.scrape_info_hashes[0], [0xAAu8; 20]);
    }

    #[tokio::test]
    async fn test_handle_scrape_response_single_hash() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let hashes = [[0x11u8; 20]];

        // Manually set up an in-flight scrape request
        let txn_id = client.next_txn();
        let mut req =
            UdpTrackerRequest::new(addr, hashes[0], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
        req.txn_id = txn_id;
        req.dispatched_at = Some(Instant::now());
        req.scrape_info_hashes = hashes.to_vec();
        client.inflight.push_back(req);
        client.txn_map.insert(txn_id, 0);

        // Build scrape response: action=2, txn_id, seeders=42, leechers=10, completed=999
        let mut resp_data = vec![0u8; 20];
        resp_data[0..4].copy_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
        resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
        resp_data[8..12].copy_from_slice(&42u32.to_be_bytes());
        resp_data[12..16].copy_from_slice(&10u32.to_be_bytes());
        resp_data[16..20].copy_from_slice(&999u32.to_be_bytes());

        client.handle_response(&resp_data, &addr).await;

        let scrape_results = client.completed_scrape_results();
        assert_eq!(scrape_results.len(), 1);
        assert_eq!(scrape_results[0].len(), 1);
        assert_eq!(scrape_results[0][0].seeders, 42);
        assert_eq!(scrape_results[0][0].leechers, 10);
        assert_eq!(scrape_results[0][0].completed, 999);
    }

    #[tokio::test]
    async fn test_handle_scrape_response_multi_hash() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
        let hashes = [[0x22u8; 20], [0x33u8; 20]];

        let txn_id = client.next_txn();
        let mut req =
            UdpTrackerRequest::new(addr, hashes[0], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
        req.txn_id = txn_id;
        req.dispatched_at = Some(Instant::now());
        req.scrape_info_hashes = hashes.to_vec();
        client.inflight.push_back(req);
        client.txn_map.insert(txn_id, 0);

        // Response for 2 info hashes
        let mut resp_data = vec![0u8; 32]; // 8 header + 12*2
        resp_data[0..4].copy_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
        resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
        // Hash 1
        resp_data[8..12].copy_from_slice(&100u32.to_be_bytes());
        resp_data[12..16].copy_from_slice(&50u32.to_be_bytes());
        resp_data[16..20].copy_from_slice(&200u32.to_be_bytes());
        // Hash 2
        resp_data[20..24].copy_from_slice(&5u32.to_be_bytes());
        resp_data[24..28].copy_from_slice(&3u32.to_be_bytes());
        resp_data[28..32].copy_from_slice(&7u32.to_be_bytes());

        client.handle_response(&resp_data, &addr).await;

        let scrape_results = client.completed_scrape_results();
        assert_eq!(scrape_results.len(), 1);
        assert_eq!(scrape_results[0].len(), 2);
        assert_eq!(scrape_results[0][0].seeders, 100);
        assert_eq!(scrape_results[0][1].seeders, 5);
    }

    #[tokio::test]
    async fn test_scrape_error_action_returns_error() {
        let mut client = UdpTrackerClient::new(0).await.unwrap();
        let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();

        let txn_id = client.next_txn();
        let mut req =
            UdpTrackerRequest::new(addr, [0x99u8; 20], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
        req.txn_id = txn_id;
        req.dispatched_at = Some(Instant::now());
        req.scrape_info_hashes = vec![[0x99u8; 20]];
        client.inflight.push_back(req);
        client.txn_map.insert(txn_id, 0);

        // Send error action instead of scrape action
        let mut err_data = vec![0u8; 23];
        err_data[0..4].copy_from_slice(&3i32.to_be_bytes()); // Error action
        err_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
        err_data[8..23].copy_from_slice(b"scrape failed!!");

        client.handle_response(&err_data, &addr).await;

        // Should not have successful scrape results
        let scrape_results = client.completed_scrape_results();
        assert!(
            scrape_results.is_empty(),
            "Error response should not produce scrape results"
        );
    }
}
