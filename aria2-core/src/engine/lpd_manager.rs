//! LPD (Local Peer Discovery) 管理器 - BEP 14 规范实现
//!
//! 通过 UDP 组播在局域网内广播活跃下载信息，让同一局域网的 peer 可以互相发现对方。
//!
//! # 协议规范 (BEP 14)
//!
//! LPD Announce 报文格式:
//! ```text
//! ┌──────────────┬──────────────┬──────────┬───────┐
//! │ InfoHash(20) │ InfoHash(20) │ Port(2B) │ 0(1B) │
//! │   (from)     │   (to)       │ (BE)     │ (\n)  │
//! └──────────────┴──────────────┴──────────┴───────┘
//! 总计: 43 字节
//! ```

use std::net::SocketAddrV4;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};

/// LPD 组播地址 (IPv4)
pub const LPD_MULTICAST_ADDR: &str = "239.192.152.143";

/// LPD 组播端口
pub const LPD_MULTICAST_PORT: u16 = 6771;

/// 默认 announce 间隔（秒）
pub const DEFAULT_ANNOUNCE_INTERVAL_SECS: u64 = 300;

/// Peer TTL（秒），超过此时间未更新的 peer 将被清除
pub const PEER_TTL_SECS: u64 = 1800;

/// LPD Announce 报文最小长度（字节）
pub const LPD_ANNOUNCE_MIN_SIZE: usize = 43;

/// LPD Announce 报文
///
/// 表示一个 LPD 广播报文，包含发送方和接收方的 info_hash 以及监听端口。
#[derive(Debug, Clone)]
pub struct LpdAnnounce {
    /// 发送方 info_hash（当前下载的 hash）
    pub from_hash: [u8; 20],
    /// 接收方匹配的 info_hash（目标下载的 hash）
    pub to_hash: [u8; 20],
    /// 监听端口（Big Endian）
    pub port: u16,
}

impl LpdAnnounce {
    /// 序列化为 UDP 报文
    ///
    /// 返回 43 字节的最小报文格式：
    /// - from_hash: 20 字节
    /// - to_hash: 20 字节
    /// - port: 2 字节（Big Endian）
    /// - 终止符: 1 字节（0x00）
    #[inline]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(LPD_ANNOUNCE_MIN_SIZE);
        buf.extend_from_slice(&self.from_hash); // 20 bytes
        buf.extend_from_slice(&self.to_hash); // 20 bytes
        buf.extend_from_slice(&self.port.to_be_bytes()); // 2 bytes (Big Endian)
        buf.push(0); // 1 byte terminator (\0)
        buf
    }

    /// 从 UDP 数据包解析 LpdAnnounce
    ///
    /// # 参数
    /// - `data`: 原始 UDP 数据包（至少 43 字节）
    ///
    /// # 返回
    /// 成功返回 `Some(LpdAnnounce)`，数据不足返回 `None`
    #[inline]
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < LPD_ANNOUNCE_MIN_SIZE {
            return None;
        }

        let mut from_hash = [0u8; 20];
        let mut to_hash = [0u8; 20];
        from_hash.copy_from_slice(&data[0..20]);
        to_hash.copy_from_slice(&data[20..40]);

        let port = u16::from_be_bytes([data[40], data[41]]);

        Some(LpdAnnounce {
            from_hash,
            to_hash,
            port,
        })
    }
}

/// 通过 LPD 发现的 peer
///
/// 记录了发现的 IPv4 地址、发现时间和关联的 info_hash。
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct LpdPeer {
    /// IPv4 地址 + 端口
    pub addr: SocketAddrV4,
    /// 发现时间戳
    pub discovered_at: Instant,
    /// 关联的 info_hash（来自报文的 to_hash）
    pub source_hash: [u8; 20],
}

impl LpdPeer {
    /// 检查此 peer 是否已过期
    #[inline]
    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.discovered_at.elapsed() > ttl
    }
}

/// LPD 管理器
///
/// 负责管理 LPD 协议的广播、监听和已发现 peer 的维护。
///
/// # 使用示例
///
/// ```ignore
/// use aria2_core::engine::lpd_manager::LpdManager;
///
/// // 创建管理器实例
/// let manager = LpdManager::new(true, 6881);
///
/// // 注册需要广播的下载
/// manager.register_download(info_hash);
///
/// // 启动后台任务
/// manager.start_announcer(tx).await;
/// manager.start_listener(rx).await;
/// ```
pub struct LpdManager {
    /// 是否启用 LPD
    enabled: std::sync::atomic::AtomicBool,
    /// announce 间隔（秒）
    announce_interval: std::sync::atomic::AtomicU64,
    /// 当前活跃的下载列表（info_hash 列表）
    active_downloads: Arc<RwLock<Vec<[u8; 20]>>>,
    /// 已发现的 peer 列表
    discovered_peers: Arc<RwLock<Vec<LpdPeer>>>,
    /// BT 监听端口
    listen_port: u16,
}

impl LpdManager {
    /// 创建新的 LPD 管理器实例
    ///
    /// # 参数
    /// - `enable`: 是否启用 LPD 功能
    /// - `port`: BitTorrent 监听端口
    ///
    /// # 示例
    ///
    /// ```ignore
    /// let manager = LpdManager::new(true, 6881);
    /// ```
    pub fn new(enable: bool, port: u16) -> Self {
        LpdManager {
            enabled: std::sync::atomic::AtomicBool::new(enable),
            announce_interval: std::sync::atomic::AtomicU64::new(DEFAULT_ANNOUNCE_INTERVAL_SECS),
            active_downloads: Arc::new(RwLock::new(Vec::new())),
            discovered_peers: Arc::new(RwLock::new(Vec::new())),
            listen_port: port,
        }
    }

    /// 注册一个需要广播的下载
    ///
    /// 将 info_hash 添加到活跃下载列表中。如果该 hash 已存在则不会重复添加。
    ///
    /// # 参数
    /// - `info_hash`: 下载的 SHA-1 info_hash（20 字节）
    pub fn register_download(&self, info_hash: [u8; 20]) {
        if !self.is_enabled() {
            debug!("LPD 未启用，忽略注册请求");
            return;
        }

        tokio::spawn({
            let downloads = self.active_downloads.clone();
            async move {
                let mut downloads = downloads.write().await;
                if !downloads.contains(&info_hash) {
                    downloads.push(info_hash);
                    info!(
                        info_hash = %hex::encode(info_hash),
                        "LPD 注册新下载"
                    );
                }
            }
        });
    }

    /// 注销已完成的下载
    ///
    /// 从活跃下载列表中移除指定的 info_hash。
    ///
    /// # 参数
    /// - `info_hash`: 要注销的 info_hash
    pub fn unregister_download(&self, info_hash: [u8; 20]) {
        tokio::spawn({
            let downloads = self.active_downloads.clone();
            async move {
                let mut downloads = downloads.write().await;
                if let Some(pos) = downloads.iter().position(|h| *h == info_hash) {
                    downloads.remove(pos);
                    info!(
                        info_hash = %hex::encode(info_hash),
                        "LPD 注销下载"
                    );
                }
            }
        });
    }

    /// 构造当前所有活跃下载的 announce 报文列表
    ///
    /// 为每个活跃的下载生成一个 LpdAnnounce 报文，用于后续广播。
    ///
    /// # 返回
    /// 所有活跃下载对应的 announce 报文向量
    pub async fn build_announces(&self) -> Vec<LpdAnnounce> {
        let downloads = self.active_downloads.read().await;
        downloads
            .iter()
            .map(|hash| LpdAnnounce {
                from_hash: *hash,
                to_hash: *hash,
                port: self.listen_port,
            })
            .collect()
    }

    /// 处理接收到的 LPD 报文
    ///
    /// 从 listener 任务调用，解析收到的 UDP 数据包并更新已发现 peer 列表。
    ///
    /// # 参数
    /// - `data`: 原始 UDP 数据包
    /// - `src_addr`: 发送方地址（IPv4）
    pub async fn handle_incoming_packet(&self, data: &[u8], src_addr: SocketAddrV4) {
        if !self.is_enabled() {
            return;
        }

        // 解析报文
        let announce = match LpdAnnounce::from_bytes(data) {
            Some(a) => a,
            None => {
                warn!(
                    src_addr = %src_addr,
                    data_len = data.len(),
                    "收到无效的 LPD 报文"
                );
                return;
            }
        };

        // 检查 to_hash 是否匹配我们正在跟踪的任何下载
        let is_matching = {
            let downloads = self.active_downloads.read().await;
            downloads.contains(&announce.to_hash)
        };

        if !is_matching {
            debug!(
                to_hash = %hex::encode(announce.to_hash),
                src_addr = %src_addr,
                "LPD 报文的 to_hash 不匹配任何活跃下载"
            );
            return;
        }

        // 构造 peer 并添加到列表
        let peer_addr = SocketAddrV4::new(*src_addr.ip(), announce.port);
        let peer_source_hash = announce.to_hash;

        // 避免重复添加相同地址的 peer
        let mut peers = self.discovered_peers.write().await;
        let exists = peers
            .iter()
            .any(|p| p.addr == peer_addr && p.source_hash == peer_source_hash);

        if !exists {
            let peer = LpdPeer {
                addr: peer_addr,
                discovered_at: Instant::now(),
                source_hash: peer_source_hash,
            };
            peers.push(peer);
            info!(
                addr = %peer_addr,
                source_hash = %hex::encode(peer_source_hash),
                "LPD 发现新 peer"
            );
        } else {
            // 更新已有 peer 的发现时间
            if let Some(existing) = peers
                .iter_mut()
                .find(|p| p.addr == peer_addr && p.source_hash == peer_source_hash)
            {
                existing.discovered_at = Instant::now();
            }
        }
    }

    /// 获取指定 info_hash 对应的所有已发现 peers
    ///
    /// # 参数
    /// - `info_hash`: 要查询的 info_hash
    ///
    /// # 返回
    /// 匹配指定 info_hash 的所有已发现 peer 列表
    pub async fn get_discovered_peers(&self, info_hash: [u8; 20]) -> Vec<LpdPeer> {
        let peers = self.discovered_peers.read().await;
        peers
            .iter()
            .filter(|p| p.source_hash == info_hash)
            .cloned()
            .collect()
    }

    /// 清除过期的 peer 条目
    ///
    /// 移除超过 TTL 的 peer 条目，保持列表的时效性。
    ///
    /// # 参数
    /// - `ttl`: 存活时间阈值
    pub async fn cleanup_expired_peers(&self, ttl: Duration) {
        let before_count = {
            let peers = self.discovered_peers.read().await;
            peers.len()
        };

        let mut peers = self.discovered_peers.write().await;
        let initial_len = peers.len();
        peers.retain(|p| !p.is_expired(ttl));
        let removed = initial_len - peers.len();

        if removed > 0 {
            info!(removed, remaining = peers.len(), "LPD 清除过期 peer 条目");
        } else if before_count > 0 {
            debug!(before_count, "LPD 无需清理过期条目");
        }
    }

    /// 启动后台 announce 任务
    ///
    /// 定时向组播地址广播所有活跃下载的信息。通过 channel 发送序列化后的报文数据。
    ///
    /// # 参数
    /// - `tx`: 用于发送报文数据的 mpsc sender
    ///
    /// # 注意
    /// 此函数会 spawn 一个后台任务并立即返回。任务会持续运行直到 tx 关闭或 LPD 被禁用。
    pub async fn start_announcer(&self, tx: mpsc::Sender<Vec<u8>>) {
        if !self.is_enabled() {
            warn!("LPD 未启用，无法启动 announcer");
            return;
        }

        let interval_secs = self
            .announce_interval
            .load(std::sync::atomic::Ordering::Relaxed);
        let active_downloads = self.active_downloads.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            interval.tick().await; // 跳过第一次立即触发

            loop {
                interval.tick().await;

                // 检查是否仍启用（避免竞态）
                {
                    let downloads = active_downloads.read().await;
                    if downloads.is_empty() {
                        continue;
                    }

                    // 构建并发送所有 announce 报文
                    for hash in downloads.iter() {
                        let announce = LpdAnnounce {
                            from_hash: *hash,
                            to_hash: *hash,
                            port: 0, // 端口由外部设置
                        };
                        let data = announce.to_bytes();

                        if tx.send(data).await.is_err() {
                            info!("LPD announce channel 已关闭，停止广播");
                            return;
                        }
                    }

                    debug!(count = downloads.len(), "LPD 发送 announce 广播");
                }
            }
        });

        info!(interval_secs, "LPD announcer 已启动");
    }

    /// 启动后台监听任务
    ///
    /// 从 channel 接收 UDP 数据包并处理。每个接收到的数据包都会被解析并更新 peer 列表。
    ///
    /// # 参数
    /// - `rx`: 用于接收 `(数据, 来源地址)` 的 mpsc receiver
    ///
    /// # 注意
    /// 此函数会 spawn 一个后台任务并立即返回。任务会持续运行直到 rx 关闭。
    pub async fn start_listener(&self, mut rx: mpsc::Receiver<(Vec<u8>, std::net::SocketAddr)>) {
        if !self.is_enabled() {
            warn!("LPD 未启用，无法启动 listener");
            return;
        }

        let discovered_peers = self.discovered_peers.clone();
        let active_downloads = self.active_downloads.clone();

        tokio::spawn(async move {
            while let Some((data, src_addr)) = rx.recv().await {
                // 只处理 IPv4 地址
                if let std::net::SocketAddr::V4(v4_addr) = src_addr {
                    // 解析报文
                    let announce = match LpdAnnounce::from_bytes(&data) {
                        Some(a) => a,
                        None => {
                            warn!(
                                src_addr = %v4_addr,
                                data_len = data.len(),
                                "收到无效的 LPD 报文"
                            );
                            continue;
                        }
                    };

                    // 检查是否匹配活跃下载
                    let is_matching = {
                        let downloads = active_downloads.read().await;
                        downloads.contains(&announce.to_hash)
                    };

                    if !is_matching {
                        continue;
                    }

                    // 添加/更新 peer
                    let peer_addr = SocketAddrV4::new(*v4_addr.ip(), announce.port);
                    let peer_source_hash = announce.to_hash;

                    let mut peers = discovered_peers.write().await;
                    let exists = peers
                        .iter()
                        .any(|p| p.addr == peer_addr && p.source_hash == peer_source_hash);

                    if !exists {
                        let peer = LpdPeer {
                            addr: peer_addr,
                            discovered_at: Instant::now(),
                            source_hash: peer_source_hash,
                        };
                        peers.push(peer);
                        debug!(
                            addr = %peer_addr,
                            "LPD listener 发现新 peer"
                        );
                    } else if let Some(existing) = peers
                        .iter_mut()
                        .find(|p| p.addr == peer_addr && p.source_hash == peer_source_hash)
                    {
                        existing.discovered_at = Instant::now();
                    }
                }
            }

            info!("LPD listener channel 已关闭");
        });

        info!("LPD listener 已启动");
    }

    /// 检查 LPD 是否已启用
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 设置 LPD 启用状态
    ///
    /// # 参数
    /// - `enabled`: 是否启用
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        info!(enabled, "LPD 状态已更新");
    }

    /// 获取当前 announce 间隔（秒）
    #[inline]
    pub fn announce_interval(&self) -> u64 {
        self.announce_interval
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 设置 announce 间隔
    ///
    /// # 参数
    /// - `secs`: 新的间隔值（秒）
    pub fn set_announce_interval(&self, secs: u64) {
        self.announce_interval
            .store(secs.max(60), std::sync::atomic::Ordering::Relaxed);
    }

    /// 获取 BT 监听端口
    #[inline]
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    /// 获取当前活跃下载数量
    pub async fn active_download_count(&self) -> usize {
        self.active_downloads.read().await.len()
    }

    /// 获取已发现 peer 数量
    pub async fn discovered_peer_count(&self) -> usize {
        self.discovered_peers.read().await.len()
    }
}
