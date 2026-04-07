use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout as tokio_timeout};
use std::time::Duration;
use tracing::{info, error, debug};

use crate::error::Result;
use super::command::{Command, CommandStatus};

pub struct DownloadEngine {
    command_tx: mpsc::UnboundedSender<Box<dyn Command>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
    tick_interval: Duration,
}

impl DownloadEngine {
    pub fn new(tick_interval_ms: u64) -> Self {
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let engine = DownloadEngine {
            command_tx,
            shutdown_tx: Some(shutdown_tx),
            tick_interval: Duration::from_millis(tick_interval_ms),
            shutdown_rx: Some(shutdown_rx),
        };

        info!("下载引擎初始化完成, tick间隔: {}ms", tick_interval_ms);

        engine
    }

    pub fn add_command(&self, command: Box<dyn Command>) -> Result<()> {
        self.command_tx.send(command)
            .map_err(|e| crate::error::Aria2Error::DownloadFailed(format!("添加命令失败: {}", e)))
    }

    pub async fn run(mut self) -> Result<()> {
        info!("下载引擎启动");

        let (internal_command_tx, mut internal_command_rx) = mpsc::unbounded_channel::<Box<dyn Command>>();
        let mut pending_commands: Vec<Box<dyn Command>> = Vec::new();
        let mut running_commands: Vec<Box<dyn Command>> = Vec::new();

        let mut ticker = interval(self.tick_interval);
        let mut shutdown_rx = self.shutdown_rx.take()
            .expect("shutdown_rx should exist in run()");

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    debug!("引擎tick触发");
                    
                    while let Ok(cmd) = internal_command_rx.try_recv() {
                        pending_commands.push(cmd);
                        debug!("收到新命令, 待处理队列: {}", pending_commands.len());
                    }

                    self.dispatch_commands(&mut pending_commands, &mut running_commands).await?;
                    self.check_timeouts(&mut running_commands).await?;
                    self.collect_completed(&mut running_commands).await?;

                    if pending_commands.is_empty() && running_commands.is_empty() {
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

        info!("下载引擎停止");
        Ok(())
    }

    async fn dispatch_commands(
        &self,
        pending: &mut Vec<Box<dyn Command>>,
        running: &mut Vec<Box<dyn Command>>
    ) -> Result<()> {
        if !pending.is_empty() {
            let cmd = pending.remove(0);
            running.push(cmd);
            debug!("调度命令, 运行中: {}", running.len());
        }
        Ok(())
    }

    async fn check_timeouts(&self, running: &mut Vec<Box<dyn Command>>) -> Result<()> {
        for cmd in running.iter_mut() {
            if let Some(timeout_dur) = cmd.timeout() {
                match tokio_timeout(timeout_dur, async {}).await {
                    Err(_) => {
                        error!("命令执行超时");
                    }
                    Ok(_) => {}
                }
            }
        }
        Ok(())
    }

    async fn collect_completed(&self, running: &mut Vec<Box<dyn Command>>) -> Result<()> {
        running.retain(|cmd| {
            matches!(cmd.status(), CommandStatus::Running | CommandStatus::Pending)
        });
        Ok(())
    }

    async fn shutdown(&self, running: &mut Vec<Box<dyn Command>>) {
        info!("正在关闭运行中的命令...");
        running.clear();
    }

    pub async fn shutdown_engine(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}
