use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::protocol::UdpTrackerRequest;
use super::{ConnectionState, UdpTrackerClient, MAX_RETRIES, REQUEST_TIMEOUT_SECS};
use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::{
    ConnectResponse, UdpAction, UdpError, UdpEvent, UdpState, build_announce_request,
    build_connect_request, parse_announce_response, parse_connect_response, parse_scrape_response,
};

impl UdpTrackerClient {
    pub async fn process_one(&mut self) -> bool {
        loop {
            if self.pending.is_empty() && self.waiting_for_conn.is_empty() {
                return false;
            }

            if let Some(mut req) = self.pending.pop_front() {
                let host_key = req.remote_addr;

                if let Some(conn) = self.conn_cache.get(&host_key) {
                    if conn.updated_at.elapsed().as_secs()
                        < aria2_protocol::bittorrent::tracker::udp_tracker_protocol::CONNECTION_TIMEOUT_SECS
                    {
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

    pub(crate) async fn send_announce(&mut self, req: &mut UdpTrackerRequest, conn_id: u64) -> bool {
        let txn_id = self.next_txn();
        req.txn_id = txn_id;
        req.dispatched_at = Some(std::time::Instant::now());
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

    pub(crate) async fn send_connect(&mut self, addr: SocketAddr) -> bool {
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
                dummy_req.dispatched_at = Some(std::time::Instant::now());
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
                            updated_at: std::time::Instant::now(),
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
        let now = std::time::Instant::now();
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
            .filter(|(_, s)| {
                s.updated_at.elapsed().as_secs()
                    > aria2_protocol::bittorrent::tracker::udp_tracker_protocol::CONNECTION_TIMEOUT_SECS
            })
            .map(|(&a, _)| a)
            .collect();

        for addr in stale_addrs {
            self.conn_cache.remove(&addr);
            debug!("Removed stale connection cache for {}", addr);
        }
    }

    pub(crate) fn is_connecting_to(&self, addr: &SocketAddr) -> bool {
        self.inflight
            .iter()
            .any(|r| &r.remote_addr == addr && r.reply.is_none())
    }

    pub fn socket(&self) -> Arc<tokio::net::UdpSocket> {
        Arc::clone(&self.socket)
    }
}
