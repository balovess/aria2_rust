use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use async_trait::async_trait;
use tracing::debug;

use crate::error::Result;
use crate::engine::command::{Command, CommandStatus};
use crate::request::request_group_man::RequestGroupMan;
use super::save_session_command::SaveSessionCommand;

pub struct AutoSaveSession {
    inner: SaveSessionCommand,
    interval: Duration,
    last_saved: Instant,
    dirty: AtomicBool,
    status: CommandStatus,
}

impl AutoSaveSession {
    pub fn new(
        path: PathBuf,
        interval: Duration,
        man: Arc<RwLock<RequestGroupMan>>,
    ) -> Self {
        AutoSaveSession {
            inner: SaveSessionCommand::new(path, man),
            interval,
            last_saved: Instant::now(),
            dirty: AtomicBool::new(false),
            status: CommandStatus::Pending,
        }
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        debug!("AutoSaveSession 标记为 dirty");
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }
}

#[async_trait]
impl Command for AutoSaveSession {
    async fn execute(&mut self) -> Result<()> {
        let elapsed = self.last_saved.elapsed();

        if elapsed < self.interval {
            debug!(
                "AutoSave 跳过: 间隔未到 ({:.1}s < {:.1}s)",
                elapsed.as_secs_f64(),
                self.interval.as_secs_f64()
            );
            return Ok(());
        }

        if !self.is_dirty() {
            debug!("AutoSave 跳过: 无变更 (dirty=false)");
            return Ok(());
        }

        debug!(
            "AutoSave 触发: 间隔={:.1}s, dirty=true",
            elapsed.as_secs_f64()
        );

        self.inner.execute().await?;
        self.last_saved = Instant::now();
        self.dirty.store(false, Ordering::SeqCst);
        self.status = self.inner.status();
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        self.status.clone()
    }

    fn priority(&self) -> u32 {
        99
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::DownloadOptions;

    #[tokio::test]
    async fn test_auto_save_creation() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let auto = AutoSaveSession::new(
            PathBuf::from("/tmp/auto.sess"),
            Duration::from_secs(10),
            man,
        );
        assert_eq!(auto.status(), CommandStatus::Pending);
        assert_eq!(auto.interval(), Duration::from_secs(10));
        assert!(!auto.is_dirty());
    }

    #[tokio::test]
    async fn test_auto_save_dirty_flag() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let auto = AutoSaveSession::new(
            PathBuf::from("/tmp/auto.sess"),
            Duration::from_secs(10),
            man,
        );

        assert!(!auto.is_dirty());
        auto.mark_dirty();
        assert!(auto.is_dirty());
    }

    #[tokio::test]
    async fn test_auto_save_skip_when_not_dirty() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_autosave_clean_{}.sess", std::process::id()));

        let mut auto = AutoSaveSession::new(
            path.clone(),
            Duration::from_secs(0),
            man,
        );

        auto.execute().await.unwrap();
        assert!(!path.exists(), "非 dirty 不应写入文件");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_auto_save_skip_when_interval_not_reached() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_autosave_interval_{}.sess", std::process::id()));

        let mut auto = AutoSaveSession::new(
            path.clone(),
            Duration::from_secs(999999),
            man,
        );
        auto.mark_dirty();

        auto.execute().await.unwrap();
        assert!(!path.exists(), "间隔未到不应写入文件");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_auto_save_triggers_on_both_conditions() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        man.write().await.add_group(
            vec!["http://example.com/auto.bin".into()],
            DownloadOptions { split: Some(2), ..Default::default() },
        ).await.unwrap();

        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_autosave_trigger_{}.sess", std::process::id()));

        let mut auto = AutoSaveSession::new(
            path.clone(),
            Duration::from_secs(0),
            man,
        );
        auto.mark_dirty();

        auto.execute().await.unwrap();
        assert!(path.exists(), "满足间隔+dirty 条件应写入文件");

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("http://example.com/auto.bin"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_auto_save_resets_dirty_after_save() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_autosave_reset_{}.sess", std::process::id()));

        let mut auto = AutoSaveSession::new(
            path.clone(),
            Duration::from_secs(0),
            man,
        );
        auto.mark_dirty();
        assert!(auto.is_dirty());

        auto.execute().await.unwrap();
        assert!(!auto.is_dirty(), "保存后应重置 dirty 标记");

        let _ = tokio::fs::remove_file(&path).await;
    }
}
