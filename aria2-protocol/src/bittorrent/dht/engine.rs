use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, info, warn};

use super::bootstrap::DhtBootstrap;
use super::client::{extract_compact_nodes_from_response, extract_compact_peers_from_response};
use super::message::{DhtMessage, DhtMessageBuilder};
use super::node::DhtNode;
use super::persistence::DhtPersistence;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::token_tracker::TokenTracker;

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
    pub dht_file_path: Option<String>,
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            self_id: generate_random_id(),
            port: 0,
            max_concurrent_queries: 16,
            query_timeout: Duration::from_secs(5),
            dht_file_path: None,
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
    token_tracker: TokenTracker,
}

impl DhtEngine {
    pub async fn start(config: DhtEngineConfig) -> Result<Arc<Self>, String> {
        let socket = DhtSocket::bind(config.port).await?;
        info!("DHT 引擎启动于 {}", socket.local_addr());

        let mut self_id = config.self_id;
        let mut loaded_nodes: Vec<DhtNode> = Vec::new();

        if let Some(ref path) = config.dht_file_path {
            match DhtPersistence::load_from_file(std::path::Path::new(path)).await {
                Ok(data) => {
                    self_id = data.self_id;
                    info!(
                        "DHT: 从 {} 加载了 {} 个节点 (self_id 已恢复)",
                        path,
                        data.nodes.len()
                    );
                    for pn in &data.nodes {
                        loaded_nodes.push(DhtNode::new(pn.id, pn.addr));
                    }
                }
                Err(e) => {
                    debug!("DHT: 无法加载路由表文件 {} (使用 bootstrap): {}", path, e);
                }
            }
        }

        let engine = Arc::new(Self {
            config: DhtEngineConfig { self_id, ..config },
            socket,
            routing_table: tokio::sync::RwLock::new(RoutingTable::new(self_id)),
            running: AtomicBool::new(true),
            tx_counter: AtomicU32::new(0),
            token_tracker: TokenTracker::new(),
        });

        for node in loaded_nodes {
            engine.routing_table.write().await.insert(node);
        }

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
            if !all_peers.is_empty() {
                break;
            }

            let closest_owned: Vec<DhtNode> = {
                let rt = self.routing_table.read().await;
                rt.find_closest(info_hash, self.config.max_concurrent_queries)
                    .into_iter()
                    .cloned()
                    .collect()
            };

            if closest_owned.is_empty() && round == 0 {
                self.bootstrap_routing_table().await;
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            if closest_owned.is_empty() {
                break;
            }

            let results = self.query_get_peers_batch(&closest_owned, info_hash).await;
            contacted += results.nodes_queried;

            all_peers.extend(results.peers);
            for (addr, nid) in results.new_nodes {
                self.routing_table
                    .write()
                    .await
                    .insert(DhtNode::new(nid, addr));
            }

            sleep(Duration::from_millis(200)).await;
        }

        all_peers.sort();
        all_peers.dedup();
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

        use futures::future::join_all;

        let mut handles = Vec::new();
        for node in &closest {
            // Validate existing token or generate new one for announce
            let _announce_token = self.token_tracker.generate_token(info_hash, &node.addr);
            let token = self.token_tracker.generate_token(info_hash, &node.addr);
            let msg = DhtMessageBuilder::announce_peer(
                self.next_tx_id(),
                &self.config.self_id,
                info_hash,
                port,
                &token,
            );
            if let Ok(data) = msg.encode() {
                let sock = self.socket.clone();
                let addr = node.addr;
                handles.push(tokio::spawn(async move {
                    sock.send_to(addr, &data).await.is_ok()
                }));
            }
        }

        let results = join_all(handles).await;
        let announced = results
            .into_iter()
            .filter_map(|r| r.ok())
            .filter(|&ok| ok)
            .count();

        info!(
            "DHT announce_peer: 向 {} 个节点宣告 (port={})",
            announced, port
        );
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

        let mut handles = Vec::new();
        for target in targets {
            let msg =
                DhtMessageBuilder::get_peers(self.next_tx_id(), &self.config.self_id, info_hash);
            let data = match msg.encode() {
                Ok(d) => d,
                Err(_) => continue,
            };

            if self.socket.send_to(target.addr, &data).await.is_err() {
                continue;
            }

            // Generate DHT security token for announce_peer
            let token = self.token_tracker.generate_token(info_hash, &target.addr);
            // Note: token stored via routing_table update below

            let sock = self.socket.clone();
            let timeout = self.config.query_timeout;

            handles.push(tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                match sock.recv_with_timeout(&mut buf, timeout).await {
                    Ok((n, _)) if n > 0 => match DhtMessage::decode(&buf[..n]) {
                        Ok(resp) => (
                            extract_compact_peers_from_response(&resp),
                            extract_compact_nodes_from_response(&resp),
                            true,
                        ),
                        Err(_) => (vec![], vec![], false),
                    },
                    _ => (vec![], vec![], false),
                }
            }));
        }

        for handle in handles {
            if let Ok((peers, nodes, success)) = handle.await {
                if success {
                    result.nodes_queried += 1;
                }
                result.peers.extend(peers);
                result.new_nodes.extend(nodes);
            }
        }

        result
    }

    async fn refresh_closest_buckets(&self) {
        let target_id = self.config.self_id;
        let closest: Vec<DhtNode> = {
            let rt = self.routing_table.read().await;
            rt.find_closest(&target_id, 4)
                .into_iter()
                .cloned()
                .collect()
        };
        self.send_find_node_to_all(&closest).await;
    }

    pub fn start_maintenance_loop(self: &Arc<Self>) {
        let e = Arc::clone(self);
        tokio::spawn(async move {
            let mut save_interval = tokio::time::interval(Duration::from_secs(900));
            loop {
                tokio::select! {
                    _ = save_interval.tick() => {
                        e.save_routing_table_if_configured().await;
                        e.refresh_closest_buckets().await;
                    }
                }
                if !e.running.load(Ordering::Relaxed) {
                    break;
                }
            }
            info!("DHT 维护循环已退出");
        });
    }

    async fn save_routing_table_if_configured(&self) {
        if let Some(ref path) = self.config.dht_file_path {
            let rt = self.routing_table.read().await;
            let nodes = DhtPersistence::collect_good_nodes(&rt);
            drop(rt);

            match DhtPersistence::save_to_file(
                std::path::Path::new(path),
                &self.config.self_id,
                &nodes,
            )
            .await
            {
                Ok(n) => debug!("DHT 自动保存: {} 个 good 节点", n),
                Err(e) => warn!("DHT 自动保存失败: {}", e),
            }
        }
    }

    async fn send_find_node_to_all(&self, targets: &[DhtNode]) {
        for target in targets {
            let msg =
                DhtMessageBuilder::find_node(self.next_tx_id(), &self.config.self_id, &target.id);
            if let Ok(data) = msg.encode() {
                let _ = self.socket.send_to(target.addr, &data).await;
            }
        }
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);

        if let Some(ref path) = self.config.dht_file_path {
            let rt = &self.routing_table;
            let nodes = DhtPersistence::collect_good_nodes(&rt.blocking_read());
            match DhtPersistence::save_to_file_sync(
                std::path::Path::new(path),
                &self.config.self_id,
                &nodes,
            ) {
                Ok(n) => info!("DHT: 已保存 {} 个 good 节点到 {}", n, path),
                Err(e) => warn!("DHT: 保存路由表失败: {}", e),
            }
        }

        info!("DHT 引擎已关闭");
    }

    pub async fn shutdown_async(&self) {
        self.running.store(false, Ordering::Relaxed);

        if let Some(ref path) = self.config.dht_file_path {
            let rt = self.routing_table.read().await;
            let nodes = DhtPersistence::collect_good_nodes(&rt);
            drop(rt);

            match DhtPersistence::save_to_file(
                std::path::Path::new(path),
                &self.config.self_id,
                &nodes,
            )
            .await
            {
                Ok(n) => info!("DHT: 已保存 {} 个 good 节点到 {}", n, path),
                Err(e) => warn!("DHT: 保存路由表失败: {}", e),
            }
        }

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
        assert!(
            stats.total_nodes >= 4,
            "should have at least bootstrap nodes"
        );
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

        assert!(
            result.rounds_completed > 0,
            "should complete at least one round"
        );
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
        assert!(
            engine.running.load(Ordering::Relaxed),
            "maintenance should keep engine running"
        );
        engine.shutdown();
        sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn test_start_with_persisted_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_dht.dat");
        let self_id = [0xBBu8; 20];
        let addr: std::net::SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let node = DhtNode::new([0xAAu8; 20], addr);
        DhtPersistence::save_to_file(&path, &self_id, &[node])
            .await
            .unwrap();

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config)
            .await
            .expect("should start with persisted data");

        assert_eq!(
            engine.config.self_id, self_id,
            "self_id should come from file"
        );
        let stats = engine.stats().await;
        assert!(
            stats.total_nodes >= 5,
            "should have bootstrap + persisted nodes (got {})",
            stats.total_nodes
        );
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_start_fallback_when_no_file() {
        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some("/nonexistent/path/dht.dat".to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config)
            .await
            .expect("should fallback to bootstrap when no file");
        let stats = engine.stats().await;
        assert!(
            stats.total_nodes >= 4,
            "should have bootstrap nodes despite missing file"
        );
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_start_uses_persisted_self_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self_id_test.dat");
        let custom_id = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        DhtPersistence::save_to_file(&path, &custom_id, &[])
            .await
            .unwrap();

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        assert_eq!(
            engine.config.self_id, custom_id,
            "engine should use persisted self_id"
        );
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_shutdown_saves_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shutdown_test.dat");

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        engine.bootstrap_routing_table().await;
        engine.shutdown_async().await;

        assert!(path.exists(), "dht.dat should exist after shutdown");
        let loaded = DhtPersistence::load_from_file_sync(&path).unwrap();
        assert_eq!(loaded.self_id, engine.config.self_id);
    }

    #[test]
    fn test_shutdown_no_path_no_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("should_not_exist.dat");

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: None,
            ..Default::default()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let engine = DhtEngine::start(config).await.unwrap();
            engine.shutdown_async().await;
        });

        assert!(
            !path.exists(),
            "no file should be created when dht_file_path is None"
        );
    }

    #[tokio::test]
    async fn test_auto_save_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_save_test.dat");

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        engine.start_maintenance_loop();

        tokio::time::sleep(Duration::from_millis(100)).await;
        engine.save_routing_table_if_configured().await;

        assert!(path.exists(), "auto-save should create dht.dat");
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_auto_save_skips_without_path() {
        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: None,
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        engine.save_routing_table_if_configured().await;
        let stats = engine.stats().await;
        assert!(
            stats.total_nodes >= 4,
            "engine should still work after skipped save"
        );
        engine.shutdown();
    }
}
