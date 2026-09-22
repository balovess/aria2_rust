use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;
use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::{
    AnnounceResponse, UdpError, UdpEvent,
};

use crate::engine::udp_tracker_client::SharedUdpClient;
use crate::network::OutboundNetworkPolicy;

const DEFAULT_UDP_PORT: u16 = 6881;
const MAX_TRACKER_ANNOUNCE_TIER: usize = 2;

#[derive(Debug, Clone)]
pub struct UdpTrackerEndpoint {
    pub url: String,
    pub addr: SocketAddr,
    pub tier: u32,
}

pub struct UdpTrackerManager {
    client: SharedUdpClient,
    endpoints: Vec<UdpTrackerEndpoint>,
    current_tier: usize,
    last_announce_interval: u32,
    last_announce_time: Option<tokio::time::Instant>,
    request_timeout: Duration,
    outbound_network_policy: Option<Arc<OutboundNetworkPolicy>>,
}

impl UdpTrackerManager {
    pub async fn new(client: SharedUdpClient) -> Self {
        Self::with_policy(client, None)
    }

    pub async fn new_with_policy(
        client: SharedUdpClient,
        policy: Arc<OutboundNetworkPolicy>,
    ) -> Self {
        Self::with_policy(client, Some(policy))
    }

    fn with_policy(
        client: SharedUdpClient,
        outbound_network_policy: Option<Arc<OutboundNetworkPolicy>>,
    ) -> Self {
        Self {
            client,
            endpoints: Vec::new(),
            current_tier: 0,
            last_announce_interval: 300,
            last_announce_time: None,
            request_timeout: Duration::from_secs(
                crate::engine::udp_tracker_client::REQUEST_TIMEOUT_SECS,
            ),
            outbound_network_policy,
        }
    }

    pub fn parse_tracker_urls(&mut self, urls: &[String]) -> usize {
        let mut added = 0usize;
        #[allow(clippy::explicit_counter_loop)]
        for (tier, urls_tier) in urls.iter().enumerate() {
            for url in urls_tier.split(',') {
                let trimmed = url.trim();
                if let Some(endpoint) = Self::parse_udp_url(trimmed, tier as u32) {
                    debug!(
                        "Adding UDP tracker endpoint: {} -> {}",
                        trimmed, endpoint.addr
                    );
                    self.endpoints.push(endpoint);
                    added += 1;
                }
            }
        }
        if !self.endpoints.is_empty() {
            info!(
                "Parsed {} UDP tracker endpoints from {} URL groups",
                added,
                urls.len()
            );
        }
        added
    }

    fn parse_udp_url(url: &str, tier: u32) -> Option<UdpTrackerEndpoint> {
        let url_lower = url.to_lowercase();

        if !url_lower.starts_with("udp://") {
            return None;
        }

        let host_part = Self::udp_host_port(url)?;

        if let Some(addr) = Self::resolve_host(host_part) {
            Some(UdpTrackerEndpoint {
                url: url.to_string(),
                addr,
                tier,
            })
        } else {
            warn!("Failed to resolve UDP tracker URL: {}", url);
            None
        }
    }

    pub(crate) fn tracker_url_is_ipv6(url: &str) -> Option<bool> {
        Self::parse_udp_url(url, 0).map(|endpoint| endpoint.addr.is_ipv6())
    }

    pub(crate) async fn tracker_url_is_ipv6_with_policy(
        url: &str,
        policy: &OutboundNetworkPolicy,
    ) -> Option<bool> {
        let host_port = Self::udp_host_port(url)?;
        let (host, port) = Self::split_host_port(host_port)?;
        policy
            .resolve_udp_host(host, port)
            .await
            .ok()
            .map(|address| address.is_ipv6())
    }

    fn udp_host_port(url: &str) -> Option<&str> {
        let scheme = url.get(..6)?;
        if !scheme.eq_ignore_ascii_case("udp://") {
            return None;
        }
        let host_port = &url[6..];
        Some(if let Some(query_idx) = host_port.find('?') {
            &host_port[..query_idx]
        } else if let Some(path_idx) = host_port.find('/') {
            &host_port[..path_idx]
        } else {
            host_port
        })
    }

    fn split_host_port(host_port: &str) -> Option<(&str, u16)> {
        if let Some(bracketed) = host_port.strip_prefix('[') {
            let close = bracketed.find(']')?;
            let host = &bracketed[..close];
            let port = bracketed[close + 1..]
                .strip_prefix(':')
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_UDP_PORT);
            return Some((host, port));
        }

        if let Some((host, port)) = host_port.rsplit_once(':') {
            return Some((host, port.parse().unwrap_or(DEFAULT_UDP_PORT)));
        }
        Some((host_port, DEFAULT_UDP_PORT))
    }

    fn resolve_host(host_port: &str) -> Option<SocketAddr> {
        let (host, port_str) = if host_port.contains(':') {
            let parts: Vec<&str> = host_port.rsplitn(2, ':').collect();
            if parts.len() == 2 {
                (parts[1], parts[0])
            } else {
                (host_port, "6881")
            }
        } else {
            (host_port, "6881")
        };

        let port: u16 = port_str.parse::<u16>().unwrap_or(DEFAULT_UDP_PORT);
        let addr_str = format!("{}:{}", host, port);

        match addr_str.to_socket_addrs() {
            Ok(mut addrs) => addrs.next(),
            Err(e) => {
                warn!("DNS resolution failed for {}: {}", addr_str, e);
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn announce(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: i64,
        left: i64,
        uploaded: i64,
        event: UdpEvent,
        num_want: i32,
    ) -> Vec<AnnounceResponse> {
        if self.endpoints.is_empty() {
            debug!("No UDP tracker endpoints configured");
            return Vec::new();
        }

        let port = self.get_bind_port().await;
        let request_timeout = self.request_timeout;
        let start_tier = self
            .current_tier
            .min(self.endpoints.len().saturating_sub(1));

        for tier_offset in 0..MAX_TRACKER_ANNOUNCE_TIER {
            let tier_idx = (start_tier + tier_offset) % self.endpoints.len();
            let ep = &self.endpoints[tier_idx];

            {
                let mut c = self.client.lock().await;
                c.add_announce(
                    &ep.addr, info_hash, peer_id, downloaded, left, uploaded, event, num_want, port,
                )
                .await;
            }

            let mut c = self.client.lock().await;
            while c.process_one().await {
                if c.has_inflight() && !c.receive_next_with_timeout(request_timeout).await {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }

        let results = self.process_responses().await;
        if results.is_empty() {
            self.advance_tier();
        }

        results
    }

    pub async fn process_responses(&mut self) -> Vec<AnnounceResponse> {
        let mut c = self.client.lock().await;
        let refs = c.completed_requests();
        let results: Vec<AnnounceResponse> = refs.iter().map(|r| (*r).clone()).collect();

        if !results.is_empty() {
            self.last_announce_interval = results.first().map(|r| r.interval).unwrap_or(300);
            self.last_announce_time = Some(tokio::time::Instant::now());
            info!(
                "UDP tracker announce completed: {} responses",
                results.len()
            );
        }

        c.handle_timeouts_with_timeout(self.request_timeout).await;
        results
    }

    pub async fn last_announce_error(&self) -> Option<UdpError> {
        self.client.lock().await.completed_announce_error()
    }

    fn advance_tier(&mut self) {
        if !self.endpoints.is_empty() {
            self.current_tier = (self.current_tier + 1) % self.endpoints.len();
            debug!("Advanced to tracker tier {}", self.current_tier);
        }
    }

    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.request_timeout = timeout.max(Duration::from_secs(1));
    }

    pub async fn use_tracker_url(&mut self, url: &str) {
        if self.endpoints.len() != 1 || self.endpoints[0].url != url {
            self.endpoints.clear();
            self.current_tier = 0;
            if let Some(policy) = self.outbound_network_policy.clone() {
                if let Some(endpoint) = self.parse_udp_url_with_policy(url, 0, &policy).await {
                    self.endpoints.push(endpoint);
                }
            } else {
                self.parse_tracker_urls(&[url.to_string()]);
            }
        }
        self.client.lock().await.clear_completed_requests();
    }

    async fn parse_udp_url_with_policy(
        &self,
        url: &str,
        tier: u32,
        policy: &OutboundNetworkPolicy,
    ) -> Option<UdpTrackerEndpoint> {
        let host_port = Self::udp_host_port(url)?;
        let (host, port) = Self::split_host_port(host_port)?;
        let addr = match policy.resolve_udp_host(host, port).await {
            Ok(addr) => addr,
            Err(error) => {
                warn!(%error, %url, "Failed to resolve UDP tracker URL for the outbound policy");
                return None;
            }
        };
        Some(UdpTrackerEndpoint {
            url: url.to_string(),
            addr,
            tier,
        })
    }

    pub(crate) fn uses_tracker_url(&self, url: &str) -> bool {
        self.endpoints.len() == 1 && self.endpoints[0].url == url
    }

    pub fn get_announce_interval(&self) -> u32 {
        self.last_announce_interval
    }

    pub fn should_reannounce(&self) -> bool {
        if let Some(t) = self.last_announce_time {
            t.elapsed().as_secs() >= self.last_announce_interval as u64
        } else {
            true
        }
    }

    async fn get_bind_port(&self) -> u16 {
        let c = self.client.lock().await;
        c.socket()
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(DEFAULT_UDP_PORT)
    }

    pub fn collect_all_peers(responses: &[AnnounceResponse]) -> Vec<(String, u16)> {
        let mut all_peers = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for resp in responses {
            for peer in &resp.peers {
                if seen.insert(peer.clone()) {
                    all_peers.push(peer.clone());
                }
            }
        }

        all_peers
    }
}

impl UdpTrackerManager {
    pub async fn from_torrent_meta(
        meta: &Arc<RwLock<TorrentMeta>>,
        client: SharedUdpClient,
    ) -> Option<Self> {
        let mut mgr = Self::new(client).await;
        let m = meta.read().await;
        let urls: Vec<String> = m.announce_list.iter().flatten().cloned().collect();
        drop(m);

        if mgr.parse_tracker_urls(&urls) > 0 {
            Some(mgr)
        } else {
            None
        }
    }
}

pub type SharedUdpTrackerManager = Arc<Mutex<UdpTrackerManager>>;

impl UdpTrackerManager {
    pub fn create_shared(client: SharedUdpClient) -> SharedUdpTrackerManager {
        Arc::new(Mutex::new(Self::new_blocking(client)))
    }

    fn new_blocking(client: SharedUdpClient) -> Self {
        Self {
            client,
            endpoints: Vec::new(),
            current_tier: 0,
            last_announce_interval: 300,
            last_announce_time: None,
            request_timeout: Duration::from_secs(
                crate::engine::udp_tracker_client::REQUEST_TIMEOUT_SECS,
            ),
            outbound_network_policy: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::udp_tracker_client::UdpTrackerClient;
    use crate::network::OutboundNetworkPolicy;

    #[tokio::test]
    async fn test_manager_creation() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let mgr = UdpTrackerManager::new(shared).await;
        assert_eq!(mgr.endpoint_count(), 0);
        assert!(mgr.should_reannounce());
    }

    #[test]
    fn test_parse_udp_url_valid() {
        let ep = UdpTrackerManager::parse_udp_url("udp://127.0.0.1:6969/announce", 0);
        assert!(ep.is_some());
        let ep = ep.unwrap();
        assert_eq!(ep.tier, 0);
        assert_eq!(ep.url, "udp://127.0.0.1:6969/announce");
    }

    #[test]
    fn test_parse_udp_url_non_udp() {
        let ep = UdpTrackerManager::parse_udp_url("http://tracker.example.com:6969/announce", 0);
        assert!(ep.is_none(), "HTTP URLs should not be parsed as UDP");
    }

    #[test]
    fn test_parse_udp_url_default_port() {
        let ep = UdpTrackerManager::parse_udp_url("udp://127.0.0.1/announce", 0);
        assert!(ep.is_some());
        assert_eq!(ep.unwrap().addr.port(), 6881);
    }

    #[test]
    fn test_parse_udp_url_identifies_ipv6_endpoint() {
        assert_eq!(
            UdpTrackerManager::tracker_url_is_ipv6("udp://[::1]:6969/announce"),
            Some(true)
        );
        assert_eq!(
            UdpTrackerManager::tracker_url_is_ipv6("udp://127.0.0.1:6969/announce"),
            Some(false)
        );
    }

    #[tokio::test]
    async fn policy_tracker_resolution_selects_a_compatible_localhost_family() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let policy = Arc::new(OutboundNetworkPolicy::single(
            "127.0.0.2".parse().expect("parse IPv4 source"),
        ));
        let mut manager = UdpTrackerManager::new_with_policy(shared, policy).await;

        manager
            .use_tracker_url("udp://localhost:6969/announce")
            .await;

        assert_eq!(manager.endpoints.len(), 1);
        assert!(manager.endpoints[0].addr.ip().is_ipv4());
    }

    #[tokio::test]
    async fn test_parse_tracker_urls_mixed() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let mut mgr = UdpTrackerManager::new(shared).await;

        let urls = vec![
            "http://tracker1.example.com:6969/announce".to_string(),
            "udp://127.0.0.1:6969/announce".to_string(),
            "udp://127.0.0.1:80/announce".to_string(),
        ];

        let count = mgr.parse_tracker_urls(&urls);
        assert_eq!(count, 2, "Should parse only UDP URLs");
        assert_eq!(mgr.endpoint_count(), 2);
    }

    #[tokio::test]
    async fn test_multi_tier_parsing() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let mut mgr = UdpTrackerManager::new(shared).await;

        let urls = vec![
            "udp://127.0.0.1:6969/announce,udp://127.0.0.1:6970/announce".to_string(),
            "udp://127.0.0.1:6980/announce".to_string(),
        ];

        let count = mgr.parse_tracker_urls(&urls);
        assert_eq!(count, 3);
        assert_eq!(mgr.endpoints[0].tier, 0);
        assert_eq!(mgr.endpoints[1].tier, 0);
        assert_eq!(mgr.endpoints[2].tier, 1);
    }

    #[tokio::test]
    async fn test_should_reannounce_logic() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let mut mgr = UdpTrackerManager::new(shared).await;

        assert!(mgr.should_reannounce(), "Should reannounce initially");
        mgr.last_announce_time = Some(tokio::time::Instant::now());
        assert!(
            !mgr.should_reannounce(),
            "Should not reannounce immediately after"
        );

        mgr.last_announce_interval = 0;
        assert!(mgr.should_reannounce(), "Should reannounce with interval=0");
    }

    #[tokio::test]
    async fn test_advance_tier_wraps() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let mut mgr = UdpTrackerManager::new(shared).await;

        let urls = vec![
            "udp://127.0.0.1:6969/announce".to_string(),
            "udp://127.0.0.1:6970/announce".to_string(),
            "udp://127.0.0.1:6980/announce".to_string(),
        ];
        mgr.parse_tracker_urls(&urls);

        assert_eq!(mgr.current_tier, 0);
        mgr.advance_tier();
        assert_eq!(mgr.current_tier, 1);
        mgr.advance_tier();
        assert_eq!(mgr.current_tier, 2);
        mgr.advance_tier();
        assert_eq!(mgr.current_tier, 0, "Tier should wrap around");
    }

    #[tokio::test]
    async fn test_collect_all_peers_dedup() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let _mgr = UdpTrackerManager::new(shared).await;

        let resp1 = AnnounceResponse {
            transaction_id: 1,
            interval: 300,
            leechers: 10,
            seeders: 5,
            peers: vec![
                ("192.168.1.1".to_string(), 6881),
                ("192.168.1.2".to_string(), 6882),
            ],
        };

        let resp2 = AnnounceResponse {
            transaction_id: 2,
            interval: 200,
            leechers: 8,
            seeders: 3,
            peers: vec![
                ("192.168.1.2".to_string(), 6882),
                ("192.168.1.3".to_string(), 6883),
            ],
        };

        let all = UdpTrackerManager::collect_all_peers(&[resp1, resp2]);
        assert_eq!(all.len(), 3, "Deduplicated peers should be 3");
    }

    #[tokio::test]
    async fn test_shared_manager_creation() {
        let shared = UdpTrackerClient::create_shared(0).await.unwrap();
        let shared_mgr = UdpTrackerManager::create_shared(shared);
        assert!(shared_mgr.try_lock().is_ok());
    }
}
