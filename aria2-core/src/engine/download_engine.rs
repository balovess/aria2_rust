use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::time::{interval, timeout as tokio_timeout};
use tracing::{debug, error, info, warn};

use super::command::{Command, CommandStatus};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::constants;
use crate::dns::dns_cache::DnsCache;
use crate::ftp::FtpConnectionPool;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group_man::RequestGroupMan;
use crate::retry::{RetryPolicy, RetryStats};
use crate::session::auto_save_session::AutoSaveSession;
use crate::session::save_session_command::SaveSessionCommand;

pub struct DownloadEngine {
    command_tx: mpsc::UnboundedSender<Box<dyn Command>>,
    command_rx: mpsc::UnboundedReceiver<Box<dyn Command>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
    tick_interval: Duration,
    retry_policy: Arc<RetryPolicy>,
    retry_stats: Arc<RetryStats>,
    global_limiter: Option<RateLimiter>,
    save_session_path: Option<PathBuf>,
    save_session_interval: Option<Duration>,
    request_group_man: Option<Arc<RwLock<RequestGroupMan>>>,
    auto_save: Option<Arc<Mutex<AutoSaveSession>>>,
    /// FTP connection pool for connection reuse across FTP downloads.
    /// Created during engine initialization and passed down via dependency injection.
    ftp_pool: Arc<FtpConnectionPool>,
    /// DNS resolution cache for avoiding repeated lookups.
    /// Created during engine initialization and passed down via dependency injection.
    dns_cache: Arc<Mutex<DnsCache>>,
    /// When true, the engine stays alive even with no pending/running commands
    /// (used for RPC listen mode). The loop only exits on shutdown signal.
    keep_alive: bool,
}

impl DownloadEngine {
    pub fn new(tick_interval_ms: u64) -> Self {
        Self::with_retry_policy(tick_interval_ms, RetryPolicy::default())
    }

    pub fn with_retry_policy(tick_interval_ms: u64, policy: RetryPolicy) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let max_tries = policy.max_tries();

        let engine = DownloadEngine {
            command_tx,
            command_rx,
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
            ftp_pool: Arc::new(FtpConnectionPool::new(constants::FTP_POOL_DEFAULT_MAX_CONNECTIONS)),
            dns_cache: Arc::new(Mutex::new(DnsCache::new())),
            keep_alive: false,
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

    pub fn add_command(&self, command: Box<dyn Command>) -> Result<()> {
        self.command_tx
            .send(command)
            .map_err(|e| Aria2Error::DownloadFailed(format!("Failed to add command: {}", e)))
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

    /// Enable/disable keep-alive mode. When true, the engine stays alive even
    /// with no pending/running commands (used for RPC listen mode). The loop
    /// only exits on shutdown signal.
    pub fn set_keep_alive(&mut self, v: bool) {
        self.keep_alive = v;
    }

    /// Clone the command sender so external callers (e.g., RPC) can submit
    /// download commands to the engine loop.
    pub fn command_sender(&self) -> mpsc::UnboundedSender<Box<dyn Command>> {
        self.command_tx.clone()
    }

    /// Take the shutdown sender so an external task (e.g., Ctrl+C handler) can
    /// signal the engine to stop. Must be called before `run()`.
    pub fn take_shutdown_sender(&mut self) -> Option<oneshot::Sender<()>> {
        self.shutdown_tx.take()
    }

    pub async fn run(mut self) -> Result<()> {
        info!("Download engine started");

        let mut pending_commands: Vec<Box<dyn Command>> = Vec::new();
        let mut running_commands: Vec<Box<dyn Command>> = Vec::new();
        let mut failed_commands: Vec<(Box<dyn Command>, u32)> = Vec::new();

        let mut ticker = interval(self.tick_interval);
        let mut shutdown_rx = self
            .shutdown_rx
            .take()
            .expect("shutdown_rx should exist in run()");
        let policy = self.retry_policy.clone();
        let stats = self.retry_stats.clone();

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    debug!("Engine tick triggered");

                    for (cmd, attempt) in failed_commands.drain(..) {
                        if policy.should_retry(attempt, &Aria2Error::Recoverable(RecoverableError::Timeout)) {
                            let wait = policy.wait_duration(attempt);
                            warn!("Retrying command (attempt {}), waiting {:?}", attempt + 1, wait);
                            pending_commands.push(cmd);
                            tokio::time::sleep(wait).await;
                        } else {
                            error!("Command retry abandoned (attempted {} times)", attempt + 1);
                        }
                    }

                    self.dispatch_commands(&mut pending_commands, &mut running_commands).await?;
                    self.check_timeouts(&mut running_commands, &policy, &stats, &mut failed_commands).await?;
                    self.collect_completed(&mut running_commands).await?;

                    if !self.keep_alive && pending_commands.is_empty() && running_commands.is_empty() && failed_commands.is_empty() {
                        info!("All tasks completed, engine shutting down");
                        break;
                    }
                }

                Ok(_) = &mut shutdown_rx => {
                    info!("Shutdown signal received");
                    self.shutdown(&mut running_commands).await;
                    break;
                }
            }
        }

        info!(
            "Download engine stopped, retry stats: total={}, timeouts={}, server_errors={}, network_failures={}",
            stats.total(),
            stats.timeouts(),
            stats.server_errors(),
            stats.network_failures()
        );
        Ok(())
    }

    async fn dispatch_commands(
        &mut self,
        pending: &mut Vec<Box<dyn Command>>,
        running: &mut Vec<Box<dyn Command>>,
    ) -> Result<()> {
        while let Ok(cmd) = self.command_rx.try_recv() {
            pending.push(cmd);
        }
        while !pending.is_empty() {
            let mut cmd = pending.remove(0);
            if let Err(e) = cmd.execute().await {
                error!("Command execution failed: {}", e);
            }
            running.push(cmd);
            debug!("Dispatching command, running: {}", running.len());
        }
        Ok(())
    }

    async fn check_timeouts(
        &self,
        running: &mut Vec<Box<dyn Command>>,
        _policy: &RetryPolicy,
        stats: &RetryStats,
        failed: &mut Vec<(Box<dyn Command>, u32)>,
    ) -> Result<()> {
        let mut still_running = Vec::new();
        for cmd in running.drain(..) {
            if let Some(timeout_dur) = cmd.timeout()
                && let Err(_) = tokio_timeout(timeout_dur, async {}).await
            {
                let status = cmd.status();
                if matches!(status, CommandStatus::Running | CommandStatus::Pending) {
                    error!("Command execution timeout, will be added to retry queue");
                    stats.record_retry(&Aria2Error::Recoverable(RecoverableError::Timeout));
                    failed.push((cmd, 0));
                    continue;
                }
            }
            still_running.push(cmd);
        }
        *running = still_running;
        Ok(())
    }

    async fn collect_completed(&self, running: &mut Vec<Box<dyn Command>>) -> Result<()> {
        running.retain(|cmd| {
            matches!(
                cmd.status(),
                CommandStatus::Running | CommandStatus::Pending
            )
        });
        Ok(())
    }

    async fn shutdown(&self, running: &mut Vec<Box<dyn Command>>) {
        info!("Shutting down running commands...");
        if let (Some(path), Some(man)) = (&self.save_session_path, &self.request_group_man) {
            let mut cmd = SaveSessionCommand::new(path.clone(), man.clone());
            match cmd.execute().await {
                Ok(_) => info!("Session saved on shutdown to {}", path.display()),
                Err(e) => warn!("Failed to save session on shutdown: {}", e),
            }
        }
        running.clear();
    }

    pub async fn shutdown_engine(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}
