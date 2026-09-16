use super::*;
use crate::request::request_group::{DownloadOptions, DownloadStatus, HaltReason};

/// Build a context with no downloads queued. `keep_alive` mirrors
/// `--enable-rpc`, which is where the halt semantics used to break.
fn test_ctx(keep_alive: bool) -> EngineLoopContext {
    EngineLoopContext {
        group_man: Arc::new(RequestGroupMan::new()),
        ftp_pool: Arc::new(FtpConnectionPool::new(1)),
        dns_cache: Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
        auto_save: None,
        auto_save_dirty_signal: None,
        event_hooks: Arc::new(DownloadEventHooks::new()),
        file_alloc_man: Arc::new(tokio::sync::RwLock::new(FileAllocationMan::new())),
        keep_alive,
        server_stat_man: ServerStatMan::shared().clone(),
        server_stat_max_age: Some(Duration::from_secs(24 * 60 * 60)),
        server_stat_save_path: None,
        server_stat_save_interval: None,
        server_stat_next_save: None,
        global_limiter: None,
        #[cfg(feature = "bittorrent")]
        public_tracker_catalog: Arc::new(
            aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new(),
        ),
        #[cfg(feature = "bittorrent")]
        bt_registry: Arc::new(std::sync::RwLock::new(
            crate::engine::bt_registry::BtRegistry::new(),
        )),
        #[cfg(feature = "bittorrent")]
        bt_listener: Arc::new(crate::engine::bt_peer_listener::BtPeerListenerManager::new()),
        #[cfg(feature = "bittorrent")]
        lpd_manager: Arc::new(crate::engine::lpd_manager::LpdManager::new()),
    }
}

/// Drive the loop until it exits, failing the test if it outlives
/// `budget`. Guards the exact regression this suite exists for: a halt
/// that never converges would otherwise hang CI instead of failing.
async fn run_until_exit(
    ctx: EngineLoopContext,
    cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    budget: Duration,
) {
    let loop_fut = run_engine_loop(ctx, cmd_rx, shutdown_rx);
    tokio::time::timeout(budget, loop_fut)
        .await
        .expect("engine loop failed to terminate after halt");
}

#[path = "commands.rs"]
mod commands;
#[path = "completions.rs"]
mod completions;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "maintenance.rs"]
mod maintenance;
