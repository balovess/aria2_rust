use super::FtpConnectionPool;

/// Pool statistics for monitoring
#[derive(Debug, Clone, Default)]
pub struct PoolStats {
    /// Total connections created
    pub connections_created: u64,
    /// Total connections reused
    pub connections_reused: u64,
    /// Total connections evicted
    pub connections_evicted: u64,
    /// Total connection failures
    pub connection_failures: u64,
    /// Current pool size
    pub current_size: usize,
    /// Peak pool size
    pub peak_size: usize,
}

impl FtpConnectionPool {
    /// Get pool statistics
    pub async fn stats(&self) -> PoolStats {
        self.stats.lock().await.clone()
    }
}
