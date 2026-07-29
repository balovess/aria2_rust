use tracing::{debug, warn};

use super::protocol::UdpTrackerRequest;
use super::UdpTrackerClient;
use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::{
    UdpError, UdpEvent, UdpState, build_scrape_request,
};

impl UdpTrackerClient {
    /// Add a SCRAPE request to query statistics for one or more info hashes
    ///
    /// # Arguments
    /// * `addr` - UDP tracker socket address
    /// * `info_hashes` - Slice of 20-byte info hashes to query (max ~74 per request)
    pub async fn add_scrape(&mut self, addr: &std::net::SocketAddr, info_hashes: &[[u8; 20]]) {
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

    pub(crate) async fn send_scrape(&mut self, req: &mut UdpTrackerRequest, conn_id: u64) -> bool {
        let txn_id = self.next_txn();
        req.txn_id = txn_id;
        req.dispatched_at = Some(std::time::Instant::now());
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
                if req.fail_count < super::MAX_RETRIES {
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

    /// Get all completed scrape results from pending requests
    pub fn completed_scrape_results(&self) -> Vec<&Vec<aria2_protocol::bittorrent::tracker::udp_tracker_protocol::ScrapeResult>> {
        self.pending
            .iter()
            .filter_map(|r| r.scrape_results.as_ref())
            .collect()
    }
}
