use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::time::{interval, timeout as tokio_timeout};
use tracing::{debug, error, info, warn};

use super::command::{Command, CommandStatus};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group_man::RequestGroupMan;
use crate::retry::{RetryPolicy, RetryStats};
use crate::session::auto_save_session::AutoSaveSession;
use crate::session::save_session_command::SaveSessionCommand;

pub struct DownloadEngine {
    command_tx: mpsc::UnboundedSender<Box<dyn Command>>,
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
}

impl DownloadEngine {
    pub fn new(tick_interval_ms: u64) -> Self {
        Self::with_retry_policy(tick_interval_ms, RetryPolicy::default())
    }

    pub fn with_retry_policy(tick_interval_ms: u64, policy: RetryPolicy) -> Self {
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let max_tries = policy.max_tries();

        let engine = DownloadEngine {
            command_tx,
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
        };

        info!(
            "下载引擎初始化完成, tick间隔: {}ms, 最大重试次数: {}",
            tick_interval_ms, max_tries
        );

        engine
    }

    pub fn set_global_rate_limiter(&mut self, config: RateLimiterConfig) {
        self.global_limiter = Some(RateLimiter::new(&config));
        info!(
            "全局限速已设置: download={:?}, upload={:?}",
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
                "自动保存 session 已启用: 路径={}, 间隔={:.1}s",
                path_clone.display(),
                interval.as_secs_f64()
            );
        } else {
            info!("手动保存 session 已启用: 路径={}", path.display());
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
            .map_err(|e| Aria2Error::DownloadFailed(format!("添加命令失败: {}", e)))
    }

    pub fn retry_stats(&self) -> &RetryStats {
        &self.retry_stats
    }

    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    pub async fn run(mut self) -> Result<()> {
        info!("下载引擎启动");

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
                    debug!("引擎tick触发");

                    for (cmd, attempt) in failed_commands.drain(..) {
                        if policy.should_retry(attempt, &Aria2Error::Recoverable(RecoverableError::Timeout)) {
                            let wait = policy.wait_duration(attempt);
                            warn!("重试命令 (第{}次), 等待 {:?}", attempt + 1, wait);
                            pending_commands.push(cmd);
                            tokio::time::sleep(wait).await;
                        } else {
                            error!("命令放弃重试 (已尝试 {} 次)", attempt + 1);
                        }
                    }

                    self.dispatch_commands(&mut pending_commands, &mut running_commands).await?;
                    self.check_timeouts(&mut running_commands, &policy, &stats, &mut failed_commands).await?;
                    self.collect_completed(&mut running_commands).await?;

                    if pending_commands.is_empty() && running_commands.is_empty() && failed_commands.is_empty() {
                        info!("所有任务已完成,引擎即将关闭");
                        break;
                    }
                }

                Ok(_) = &mut shutdown_rx => {
                    info!("收到关闭信号");
                    self.shutdown(&mut running_commands).await;
                    break;
                }
            }
        }

        info!(
            "下载引擎停止, 重试统计: 总计={}, 超时={}, 服务错误={}, 网络故障={}",
            stats.total(),
            stats.timeouts(),
            stats.server_errors(),
            stats.network_failures()
        );
        Ok(())
    }

    async fn dispatch_commands(
        &self,
        pending: &mut Vec<Box<dyn Command>>,
        running: &mut Vec<Box<dyn Command>>,
    ) -> Result<()> {
        while !pending.is_empty() {
            let cmd = pending.remove(0);
            running.push(cmd);
            debug!("调度命令, 运行中: {}", running.len());
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
                    error!("命令执行超时, 将加入重试队列");
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
        info!("正在关闭运行中的命令...");
        if let (Some(path), Some(man)) = (&self.save_session_path, &self.request_group_man) {
            let mut cmd = SaveSessionCommand::new(path.clone(), man.clone());
            match cmd.execute().await {
                Ok(_) => info!("关闭时已保存 session 到 {}", path.display()),
                Err(e) => warn!("关闭时保存 session 失败: {}", e),
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
