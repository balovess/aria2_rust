use async_trait::async_trait;
use crate::error::{Aria2Error, Result};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum CommandStatus {
    Pending,
    Running,
    Completed,
    Failed(Aria2Error),
    Timeout,
}

#[async_trait]
pub trait Command: Send + Sync {
    async fn execute(&mut self) -> Result<()>;
    
    fn status(&self) -> CommandStatus;
    
    fn priority(&self) -> u32 {
        0
    }
    
    fn timeout(&self) -> Option<Duration> {
        None
    }
}
