mod lifecycle;
mod progress;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tracing::info;

#[cfg(feature = "bittorrent")]
use super::bt_registry::BtRegistry;
use super::engine_command::EngineCommand;
use crate::constants;
use crate::dns::dns_cache::DnsCache;
use crate::ftp::FtpConnectionPool;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group_man::RequestGroupMan;
use crate::retry::{RetryPolicy, RetryStats};
use crate::session::auto_save_session::AutoSaveSession;

pub struct DownloadEngine {
    /// Sender for structured engine communication commands.
    pub(crate) engine_cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    pub(crate) engine_cmd_rx: Option<mpsc::UnboundedReceiver<EngineCommand>>,
    pub(crate) shutdown_tx: Option<oneshot::Sender<()>>,
    pub(crate) shutdown_rx: Option<oneshot::Receiver<()>>,
    pub(crate) tick_interval: Duration,
    pub(crate) retry_policy: Arc<RetryPolicy>,
    pub(crate) retry_stats: Arc<RetryStats>,
    pub(crate) global_limiter: Option<RateLimiter>,
    pub(crate) save_session_path: Option<PathBuf>,
    pub(crate) save_session_interval: Option<Duration>,
    pub(crate) request_group_man: Option<Arc<RwLock<RequestGroupMan>>>,
    pub(crate) auto_save: Option<Arc<Mutex<AutoSaveSession>>>,
    /// FTP connection pool for connection reuse across FTP downloads.
    /// Created during engine initialization and passed down via dependency injection.
    pub(crate) ftp_pool: Arc<FtpConnectionPool>,
    /// DNS resolution cache for avoiding repeated lookups.
    /// Created during engine initialization and passed down via dependency injection.
    pub(crate) dns_cache: Arc<Mutex<DnsCache>>,
    /// When true, the engine stays alive even with no pending/running commands
    /// (used for RPC listen mode). The loop only exits on shutdown signal.
    pub(crate) keep_alive: bool,
    /// BitTorrent registry -- maps GID to BtObject (DownloadContext,
    /// etc.). In C++ aria2, this is a global singleton in DownloadEngine.
    /// Here it is owned by the engine and accessible via `bt_registry()`.
    /// Used for info-hash reverse lookup, peer blocklist, and BT component
    /// coordination across all active downloads.
    #[cfg(feature = "bittorrent")]
    pub(crate) bt_registry: Arc<std::sync::RwLock<BtRegistry>>,
    /// Download lifecycle event bus (shell hooks + observers).
    ///
    /// Defaults to the process-wide instance returned by
    /// [`DownloadEventHooks::shared`], so a listener registered by the binary
    /// crate before `run()` is observed by the engine loop as well as by
    /// the group state transitions inside individual commands.
    pub(crate) event_hooks: Arc<super::download_event_hooks::DownloadEventHooks>,
}

impl DownloadEngine {
    pub fn new(tick_interval_ms: u64) -> Self {
        Self::with_retry_policy(tick_interval_ms, RetryPolicy::default())
    }

    pub fn with_retry_policy(tick_interval_ms: u64, policy: RetryPolicy) -> Self {
        let (engine_cmd_tx, engine_cmd_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let max_tries = policy.max_tries();

        let engine = DownloadEngine {
            engine_cmd_tx,
            engine_cmd_rx: Some(engine_cmd_rx),
            shutdown_tx: Some(shutdown_tx),
            shutdown_rx: Some(shutdown_rx),
            tick_interval: Duration::from_millis(tick_interval_ms),
            retry_policy: Arc::new(policy),
            retry_stats: Arc::new(RetryStats::default()),
            global_limiter: None,
            save_session_path: None,
            save_session_interval: None,
            request_group_man: None,
            auto_save: None,
            ftp_pool: Arc::new(FtpConnectionPool::new(
                constants::FTP_POOL_DEFAULT_MAX_CONNECTIONS,
            )),
            dns_cache: Arc::new(Mutex::new(DnsCache::new())),
            keep_alive: false,
            #[cfg(feature = "bittorrent")]
            bt_registry: Arc::new(std::sync::RwLock::new(BtRegistry::new())),
            event_hooks: Arc::clone(super::download_event_hooks::DownloadEventHooks::shared()),
        };

        info!(
            "Download engine initialization complete, tick interval: {}ms, max retries: {}",
            tick_interval_ms, max_tries
        );

        engine
    }

    pub fn set_global_rate_limiter(&mut self, config: RateLimiterConfig) {
        self.global_limiter = Some(RateLimiter::new(&config));
        info!(
            "Global speed limits set: download={:?}, upload={:?}",
            config.download_rate(),
            config.upload_rate()
        );
    }

    pub fn global_rate_limiter(&self) -> Option<&RateLimiter> {
        self.global_limiter.as_ref()
    }

    pub fn take_global_rate_limiter(&mut self) -> Option<RateLimiter> {
        self.global_limiter.take()
    }

    pub fn set_request_group_man(&mut self, man: Arc<RwLock<RequestGroupMan>>) {
        self.request_group_man = Some(man);
    }

    pub fn set_save_session(
        &mut self,
        path: PathBuf,
        interval: Option<Duration>,
        man: Arc<RwLock<RequestGroupMan>>,
    ) {
        self.save_session_path = Some(path.clone());
        self.save_session_interval = interval;
        self.request_group_man = Some(man);

        if let (Some(interval), Some(man_ref)) = (interval, &self.request_group_man) {
            let path_clone = path.clone();
            let auto_save = AutoSaveSession::new(path, interval, man_ref.clone());
            self.auto_save = Some(Arc::new(Mutex::new(auto_save)));
            info!(
                "Auto-save session enabled: path={}, interval={:.1}s",
                path_clone.display(),
                interval.as_secs_f64()
            );
        } else {
            info!("Manual save session enabled: path={}", path.display());
        }
    }

    pub fn mark_session_dirty(&self) {
        if let Some(ref auto_save) = self.auto_save
            && let Ok(auto) = auto_save.try_lock()
        {
            auto.mark_dirty();
        }
    }

    pub fn save_session_path(&self) -> Option<&PathBuf> {
        self.save_session_path.as_ref()
    }

    pub fn retry_stats(&self) -> &RetryStats {
        &self.retry_stats
    }

    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    /// Get a reference to the FTP connection pool for dependency injection.
    pub fn ftp_pool(&self) -> &Arc<FtpConnectionPool> {
        &self.ftp_pool
    }

    /// Get a reference to the DNS cache for dependency injection.
    pub fn dns_cache(&self) -> &Arc<Mutex<DnsCache>> {
        &self.dns_cache
    }

    /// Mark one connected address as bad while retaining other resolved
    /// candidates for the same hostname.
    pub async fn mark_bad_ip_address(&self, hostname: &str, address: std::net::SocketAddr) {
        self.dns_cache.lock().await.mark_bad(hostname, address);
    }

    /// Remove cached addresses for a hostname, forcing the next connection to
    /// resolve it again.
    pub async fn remove_cached_ip_address(&self, hostname: &str, port: u16) {
        self.dns_cache.lock().await.remove_cached(hostname, port);
    }

    /// Get a reference to the BitTorrent registry.
    ///
    /// The registry maps GID to [`BtObject`](super::bt_registry::BtObject) and
    /// supports info-hash reverse lookup, peer blocklist, and BT component
    /// coordination across all active downloads. In C++ aria2, this is a global
    /// singleton owned by `DownloadEngine`.
    #[cfg(feature = "bittorrent")]
    pub fn bt_registry(&self) -> &Arc<std::sync::RwLock<BtRegistry>> {
        &self.bt_registry
    }

    /// Enable/disable keep-alive mode. When true, the engine stays alive even
    /// with no pending/running commands (used for RPC listen mode). The loop
    /// only exits on shutdown signal.
    pub fn set_keep_alive(&mut self, v: bool) {
        self.keep_alive = v;
    }

    /// Clone the EngineCommand sender so external callers (e.g., RPC) can
    /// submit structured download lifecycle commands.
    ///
    /// This is the engine interface for download management
    /// (add/remove/pause/unpause/halt).
    pub fn engine_command_sender(&self) -> mpsc::UnboundedSender<EngineCommand> {
        self.engine_cmd_tx.clone()
    }

    /// Take the shutdown sender so an external task (e.g., Ctrl+C handler) can
    /// signal the engine to stop. Must be called before `run()`.
    pub fn take_shutdown_sender(&mut self) -> Option<oneshot::Sender<()>> {
        self.shutdown_tx.take()
    }

    /// Get a clone of the engine command sender for sending commands like
    /// `ForceHaltAll` from external tasks (e.g., second Ctrl+C handler).
    pub fn engine_cmd_tx(&self) -> mpsc::UnboundedSender<EngineCommand> {
        self.engine_cmd_tx.clone()
    }

    /// Access the download lifecycle event bus.
    ///
    /// Layers above `aria2-core` use this to install a
    /// [`DownloadEventListener`](super::download_event_hooks::DownloadEventListener)
    /// **before** `run()` consumes the engine — for example the
    /// adapter in the `aria2` binary that republishes lifecycle events as
    /// JSON-RPC WebSocket notifications.
    pub fn event_hooks(&self) -> &Arc<super::download_event_hooks::DownloadEventHooks> {
        &self.event_hooks
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "bittorrent")]
    use super::DownloadEngine;
    #[cfg(feature = "bittorrent")]
    use std::sync::Arc;

    #[cfg(feature = "bittorrent")]
    /// Verify that the engine creates a BtRegistry and the accessor returns it.
    #[test]
    fn test_bt_registry_accessor_returns_valid_registry() {
        let engine = DownloadEngine::new(100);
        let registry = engine.bt_registry();
        let reg = registry.read().unwrap();
        assert!(reg.is_empty(), "new engine should have empty BtRegistry");
        assert_eq!(reg.tcp_port(), 0);
        assert_eq!(reg.udp_port(), 0);
    }

    #[cfg(feature = "bittorrent")]
    /// Verify that multiple Arc clones of the BtRegistry share the same
    /// underlying data, so changes made through one clone are visible
    /// through the other.
    #[test]
    fn test_bt_registry_arc_shared_ownership() {
        let engine = DownloadEngine::new(100);
        let registry_arc = engine.bt_registry().clone();

        // Insert via the cloned Arc
        {
            let mut reg = registry_arc.write().unwrap();
            reg.set_tcp_port(6881);
            let obj = super::super::bt_registry::BtObject::new();
            reg.put(42, obj);
        }

        // Verify visibility through the engine's accessor
        let reg = engine.bt_registry().read().unwrap();
        assert_eq!(reg.tcp_port(), 6881);
        assert!(reg.get(42).is_some());
    }

    #[cfg(feature = "bittorrent")]
    /// Verify BtRegistry info-hash lookup works end-to-end when a
    /// DownloadContext with TorrentAttribute is registered.
    #[test]
    fn test_bt_registry_info_hash_lookup_via_engine() {
        use crate::download::download_context::{
            BtFileMode, ContextAttributeType, TorrentAttribute,
        };

        let engine = DownloadEngine::new(100);
        let info_hash = "abcdef0123456789abcdef0123456789abcdef01";

        // Create a DownloadContext with TorrentAttribute
        let mut ctx =
            crate::download::DownloadContext::new(1024, 4096, "/tmp/test.bin".to_string());
        let ta = TorrentAttribute {
            name: "test_torrent".to_string(),
            mode: BtFileMode::Single,
            announce_list: vec![],
            nodes: vec![],
            info_hash: info_hash.to_string(),
            metadata: vec![],
            metadata_size: 0,
            private_torrent: false,
            creation_date: 0,
            comment: String::new(),
            created_by: String::new(),
            url_list: vec![],
        };
        ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(ta));
        let ctx = Arc::new(ctx);

        // Register into BtRegistry
        let obj = super::super::bt_registry::BtObject::builder()
            .download_context(Arc::clone(&ctx))
            .build();
        {
            let mut reg = engine.bt_registry().write().unwrap();
            reg.put(123, obj);
        }

        // Lookup by info_hash should find the context
        let reg = engine.bt_registry().read().unwrap();
        let found = reg.get_download_context_by_info_hash(info_hash);
        assert!(
            found.is_some(),
            "info-hash lookup should find registered context"
        );

        // Wrong hash should not find it
        assert!(
            reg.get_download_context_by_info_hash("wrong_hash")
                .is_none(),
            "wrong hash should not match"
        );
    }
}
