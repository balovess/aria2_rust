use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use async_trait::async_trait;
use tracing::{debug, info};

use super::session_serializer;
use crate::engine::command::{Command, CommandStatus};
use crate::error::Result;
use crate::request::request_group_man::RequestGroupMan;

pub struct SaveSessionCommand {
    path: PathBuf,
    request_group_man: Arc<RwLock<RequestGroupMan>>,
    status: CommandStatus,
}

impl SaveSessionCommand {
    pub fn new(path: PathBuf, man: Arc<RwLock<RequestGroupMan>>) -> Self {
        SaveSessionCommand {
            path,
            request_group_man: man,
            status: CommandStatus::Pending,
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[async_trait]
impl Command for SaveSessionCommand {
    async fn execute(&mut self) -> Result<()> {
        self.status = CommandStatus::Running;
        debug!("开始保存 session 到 {}", self.path.display());

        let groups = self.request_group_man.read().await.list_groups().await;
        session_serializer::save_to_file(&self.path, &groups).await?;

        self.status = CommandStatus::Completed;
        info!("Session 已保存到 {}", self.path.display());
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        self.status.clone()
    }

    fn priority(&self) -> u32 {
        100
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::DownloadOptions;

    #[tokio::test]
    async fn test_save_session_command_creation() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let cmd = SaveSessionCommand::new(PathBuf::from("/tmp/test.sess"), man);
        assert_eq!(cmd.status(), CommandStatus::Pending);
        assert!(cmd.path().to_str().unwrap().contains("test.sess"));
    }

    #[tokio::test]
    async fn test_save_session_command_execute() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        man.write()
            .await
            .add_group(
                vec!["http://example.com/file.zip".into()],
                DownloadOptions {
                    split: Some(4),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_save_session_{}.sess", std::process::id()));
        let mut cmd = SaveSessionCommand::new(path.clone(), man);

        cmd.execute().await.unwrap();
        assert_eq!(cmd.status(), CommandStatus::Completed);

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("http://example.com/file.zip"));
        assert!(content.contains("GID="));
        assert!(content.contains("split=4"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_save_session_empty_manager() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_save_empty_{}.sess", std::process::id()));
        let mut cmd = SaveSessionCommand::new(path.clone(), man);

        cmd.execute().await.unwrap();
        assert_eq!(cmd.status(), CommandStatus::Completed);

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.is_empty() || content.trim().is_empty());

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_save_session_atomic_write() {
        let man = Arc::new(RwLock::new(RequestGroupMan::new()));
        man.write()
            .await
            .add_group(
                vec!["http://example.com/atomic.bin".into()],
                DownloadOptions::default(),
            )
            .await
            .unwrap();

        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_atomic_{}.sess", std::process::id()));
        let mut cmd = SaveSessionCommand::new(path.clone(), man);

        cmd.execute().await.unwrap();

        assert!(path.exists(), "目标文件应存在");
        let tmp_path = path.with_extension("sess.tmp");
        assert!(!tmp_path.exists(), "临时文件应已被 rename 删除");

        let _ = tokio::fs::remove_file(&path).await;
    }
}
