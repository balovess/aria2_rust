use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, info};

use super::socket::DhtSocket;
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::bootstrap::DhtBootstrap;
use super::message::{DhtMessage, DhtMessageBuilder};
use super::client::{extract_compact_peers_from_response, extract_compact_nodes_from_response};

fn generate_random_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    getrandom::getrandom(&mut id).expect("generate_random_id failed");
    id[0] &= 0x03;
    id
}

pub struct DhtEngineConfig {
    pub self_id: [u8; 20],
    pub port: u16,
    pub max_concurrent_queries: usize,
    pub query_timeout: Duration,
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            self_id: generate_random_id(),
            port: 0,
            max_concurrent_queries: 16,
            query_timeout: Duration::from_secs(5),
        }
    }
}

pub struct DhtPeerDiscoveryResult {
    pub peers: Vec<std::net::SocketAddr>,
    pub nodes_contacted: usize,
    pub rounds_completed: usize,
}

pub struct DhtStats {
    pub total_nodes: usize,
    pub good_nodes: usize,
}

struct BatchQueryResult {
    peers: Vec<std::net::SocketAddr>,
    new_nodes: Vec<(std::net::SocketAddr, [u8; 20])>,
    nodes_queried: usize,
}

pub struct DhtEngine {
    config: DhtEngineConfig,
    socket: DhtSocket,
    routing_table: tokio::sync::RwLock<RoutingTable>,
    running: AtomicBool,
    tx_counter: AtomicU32,
}

impl DhtEngine {
    pub async fn start(config: DhtEngineConfig) -> Result<Arc<Self>, String> {
        let socket = DhtSocket::bind(config.port).await?;
        info!("DHT 引擎启动于 {}", socket.local_addr());

        let engine = Arc::new(Self {
            config,
            socket,
            routing_table: tokio::sync::RwLock::new(RoutingTable::new(generate_random_id())),
            running: AtomicBool::new(true),
            tx_counter: AtomicU32::new(0),
        });

        engine.bootstrap_routing_table().await;
        Ok(engine)
    }

    async fn bootstrap_routing_table(&self) {
        let boot_nodes = DhtBootstrap::get_bootstrap_nodes();
        for node in &boot_nodes {
            self.routing_table.write().await.insert(node.clone());
        }
        debug!("DHT bootstrap: 已添加 {} 个节点", boot_nodes.len());
        self.send_ping_to_all(&boot_nodes).await;
    }

    async fn send_ping_to_all(&self, nodes: &[DhtNode]) {
        for node in nodes {
            let msg = DhtMessageBuilder::ping(self.next_tx_id(), &self.config.self_id);
            if let Ok(data) = msg.encode() {
                let _ = self.socket.send_to(node.addr, &data).await;
            }
        }
    }

    pub async fn find_peers(&self, info_hash: &[u8; 20]) -> DhtPeerDiscoveryResult {
        let mut all_peers: Vec<std::net::SocketAddr> = Vec::new();
        let mut contacted = 0usize;
        const MAX_ROUNDS: usize = 3;

        for round in 0..MAX_ROUNDS {
            if !all_peers.is_empty() { break; }

            let closest_owned: Vec<DhtNode> = {
                let rt = self.routing_table.read().await;
                rt.find_closest(info_hash, self.config.max_concurrent_queries)
                    .into_iter().cloned().collect()
            };

            if closest_owned.is_empty() && round == 0 {
                self.bootstrap_routing_table().await;
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            if closest_owned.is_empty() { break; }

            let results = self.query_get_peers_batch(&closest_owned, info_hash).await;
            contacted += results.nodes_queried;

            all_peers.extend(results.peers);
            for (addr, nid) in results.new_nodes {
                self.routing_table.write().await.insert(DhtNode::new(nid, addr));
            }

            sleep(Duration::from_millis(200)).await;
        }

        all_peers.sort(); all_peers.dedup();
        let is_empty = all_peers.is_empty();

        DhtPeerDiscoveryResult {
            peers: all_peers,
            nodes_contacted: contacted,
            rounds_completed: if is_empty { MAX_ROUNDS } else { 1 },
        }
    }

    pub async fn announce_peer(&self, info_hash: &[u8; 20], port: u16) -> Result<(), String> {
        let closest: Vec<DhtNode> = {
            let rt = self.routing_table.read().await;
            rt.find_closest(info_hash, 8).into_iter().cloned().collect()
        };

        let mut announced = 0usize;
        for node in &closest {
            if let Some(ref token) = node.token {
                let msg = DhtMessageBuilder::announce_peer(
                    self.next_tx_id(), &self.config.self_id, info_hash, port, token.as_str(),
                );
                if let Ok(data) = msg.encode() {
                    if self.socket.send_to(node.addr, &data).await.is_ok() {
                        announced += 1;
                    }
                }
            }
        }

        info!("DHT announce_peer: 向 {} 个节点宣告 (port={})", announced, port);
        Ok(())
    }

    async fn query_get_peers_batch(
        &self,
        targets: &[DhtNode],
        info_hash: &[u8; 20],
    ) -> BatchQueryResult {
        let mut result = BatchQueryResult {
            peers: vec![],
            new_nodes: vec![],
            nodes_queried: 0,
        };
        let mut buf = [0u8; 8192];

        for target in targets {
            let msg = DhtMessageBuilder::get_peers(
                self.next_tx_id(), &self.config.self_id, info_hash,
            );
            let data = match msg.encode() {
                Ok(d) => d,
                Err(_) => continue,
            };

            if self.socket.send_to(target.addr, &data).await.is_err() {
                continue;
            }

            result.nodes_queried += 1;

            match self.socket.recv_with_timeout(&mut buf, self.config.query_timeout).await {
                Ok((n, _)) => {
                    if n == 0 { continue; }
                    match DhtMessage::decode(&buf[..n]) {
                        Ok(response) => {
                            result.peers.extend(extract_compact_peers_from_response(&response));
                            result.new_nodes.extend(extract_compact_nodes_from_response(&response));
                        }
                        Err(_) => {}
                    }
                }
                Err(_) => {}
            }
        }

        result
    }

    async fn refresh_closest_buckets(&self) {
        let target_id = self.config.self_id;
        let closest: Vec<DhtNode> = {
            let rt = self.routing_table.read().await;
            rt.find_closest(&target_id, 4).into_iter().cloned().collect()
        };
        self.send_find_node_to_all(&closest).await;
    }

    pub fn start_maintenance_loop(self: &Arc<Self>) {
        let e = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(900)) => {
                        e.refresh_closest_buckets().await;
                    }
                }
                if !e.running.load(Ordering::Relaxed) { break; }
            }
            info!("DHT 维护循环已退出");
        });
    }

    async fn send_find_node_to_all(&self, targets: &[DhtNode]) {
        for target in targets {
            let msg = DhtMessageBuilder::find_node(
                self.next_tx_id(), &self.config.self_id, &target.id,
            );
            if let Ok(data) = msg.encode() {
                let _ = self.socket.send_to(target.addr, &data).await;
            }
        }
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        info!("DHT 引擎已关闭");
    }

    pub async fn stats(&self) -> DhtStats {
        let rt = self.routing_table.read().await;
        DhtStats {
            total_nodes: rt.total_node_count(),
            good_nodes: rt.good_node_count(),
        }
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.socket.local_addr()
    }

    fn next_tx_id(&self) -> u32 {
        self.tx_counter.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_start_and_stats() {
        let config = DhtEngineConfig {
            port: 0,
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.expect("engine should start");
        let stats = engine.stats().await;
        assert!(stats.total_nodes >= 4, "should have at least bootstrap nodes");
        engine.shutdown();
    }

    #[tokio::test]
    async fn test_find_peers_returns_result() {
        let config = DhtEngineConfig {
            port: 0,
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.expect("engine should start");

        let hash = [0xABu8; 20];
        let result = engine.find_peers(&hash).await;

        assert!(result.rounds_completed > 0, "should complete at least one round");
        let _ = result.nodes_contacted;
        engine.shutdown();
    }

    #[test]
    fn test_config_default_values() {
        let cfg = DhtEngineConfig::default();
        assert_eq!(cfg.port, 0);
        assert_eq!(cfg.max_concurrent_queries, 16);
        assert_eq!(cfg.query_timeout, Duration::from_secs(5));
    }

    #[test]
    fn test_config_self_id_is_valid() {
        let cfg = DhtEngineConfig::default();
        assert_ne!(cfg.self_id, [0u8; 20], "self_id should not be all zeros");
    }

    #[tokio::test]
    async fn test_shutdown_sets_flag() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        assert!(engine.running.load(Ordering::Relaxed));
        engine.shutdown();
        assert!(!engine.running.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_local_addr_valid() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        let addr = engine.local_addr();
        assert!(addr.port() > 0, "should have a valid port");
        engine.shutdown();
    }

    #[tokio::test]
    async fn test_maintenance_loop_starts() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        engine.start_maintenance_loop();
        sleep(Duration::from_millis(50)).await;
        assert!(engine.running.load(Ordering::Relaxed), "maintenance should keep engine running");
        engine.shutdown();
        sleep(Duration::from_millis(200)).await;
    }
}
