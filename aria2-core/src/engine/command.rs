use crate::error::Result;
use async_trait::async_trait;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum CommandStatus {
    Pending,
    Running,
    Completed,
}

#[async_trait]
pub trait Command: Send + Sync {
    async fn execute(&mut self) -> Result<()>;

    fn status(&self) -> CommandStatus;

    fn timeout(&self) -> Option<Duration> {
        None
    }
}
