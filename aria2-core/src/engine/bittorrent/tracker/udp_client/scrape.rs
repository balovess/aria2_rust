use std::net::SocketAddr;
use std::time::Duration;

use super::request::UdpTrackerRequest;
use super::{UdpError, UdpTrackerClient};
use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::{
    ScrapeResult, UdpEvent, build_scrape_request,
};

impl UdpTrackerClient {
    /// Query tracker statistics for one or more info hashes.
    pub async fn scrape(
        &mut self,
        addr: SocketAddr,
        info_hashes: &[[u8; 20]],
        timeout: Duration,
    ) -> Result<Vec<ScrapeResult>, UdpError> {
        if info_hashes.is_empty() {
            return Ok(Vec::new());
        }

        self.add_scrape(&addr, info_hashes);
        loop {
            if let Some(index) = self.pending.iter().position(|request| {
                request.remote_addr == addr
                    && request.scrape_info_hashes == info_hashes
                    && (request.scrape_results.is_some() || request.error.is_some())
            }) {
                let request = self
                    .pending
                    .remove(index)
                    .expect("completed UDP scrape request index is valid");
                if let Some(results) = request.scrape_results {
                    return Ok(results);
                }
                return Err(request.error.unwrap_or(UdpError::MalformedResponse));
            }

            let processed = self.process_one().await;
            if self.has_inflight() {
                if !self.receive_next_with_timeout(timeout).await {
                    self.handle_timeouts_with_timeout(timeout).await;
                }
                continue;
            }
            if !processed {
                return Err(UdpError::Network);
            }
            tokio::task::yield_now().await;
        }
    }

    pub(super) fn add_scrape(&mut self, addr: &SocketAddr, info_hashes: &[[u8; 20]]) {
        let mut req = UdpTrackerRequest::new(
            *addr,
            info_hashes[0],
            [0u8; 20],
            0,
            0,
            0,
            UdpEvent::None,
            0,
            0,
        );
        req.scrape_info_hashes = info_hashes.to_vec();
        self.pending.push_back(req);
        tracing::debug!(
            "Added scrape request for {} ({} hashes)",
            addr,
            info_hashes.len()
        );
    }

    pub(crate) async fn send_scrape(&mut self, req: &mut UdpTrackerRequest, conn_id: u64) -> bool {
        let txn_id = self.next_txn();
        req.txn_id = txn_id;
        req.dispatched_at = Some(std::time::Instant::now());
        let hash_count = req.scrape_info_hashes.len();

        let payload = build_scrape_request(conn_id, txn_id, &req.scrape_info_hashes);

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
                tracing::debug!(
                    "Sent SCRAPE {} bytes to {} (txn={}, {} hashes)",
                    len,
                    req.remote_addr,
                    txn_id,
                    hash_count
                );
                true
            }
            Err(e) => {
                tracing::warn!("Send SCRAPE to {} failed: {}", req.remote_addr, e);
                req.fail_count += 1;
                let retry = req.fail_count < super::MAX_RETRIES;
                req.error = (!retry).then_some(UdpError::Network);
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
                let completed_or_retry = std::mem::replace(req, replacement);
                if retry {
                    self.pending.push_front(completed_or_retry);
                } else {
                    self.pending.push_back(completed_or_retry);
                }
                true
            }
        }
    }
}
