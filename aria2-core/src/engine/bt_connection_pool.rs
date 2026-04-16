// aria2-core/src/engine/bt_connection_pool.rs

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tracing::debug;

pub struct ReadyConnection {
    pub stream: TcpStream,
    pub addr: SocketAddr,
    ready_at: Instant,
}

impl ReadyConnection {
    pub fn new(stream: TcpStream, addr: SocketAddr) -> Self {
        Self {
            stream,
            addr,
            ready_at: Instant::now(),
        }
    }

    pub fn age(&self) -> Duration {
        self.ready_at.elapsed()
    }
    pub fn is_stale(&self, max_age: Duration) -> bool {
        self.age() > max_age
    }
}

pub struct BtConnectionPool {
    connections: VecDeque<ReadyConnection>,
    max_idle: usize,
    connect_timeout: Duration,
    stale_threshold: Duration,
}

impl BtConnectionPool {
    pub fn new() -> Self {
        Self {
            connections: VecDeque::new(),
            max_idle: 20,
            connect_timeout: Duration::from_secs(5),
            stale_threshold: Duration::from_secs(30),
        }
    }

    pub fn with_max_idle(mut self, n: usize) -> Self {
        self.max_idle = n;
        self
    }
    pub fn with_connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }
    pub fn with_stale_threshold(mut self, d: Duration) -> Self {
        self.stale_threshold = d;
        self
    }

    pub async fn prewarm(&mut self, addr: SocketAddr) -> Result<bool, String> {
        if self.contains_addr(&addr) {
            return Ok(false);
        }

        match tokio::time::timeout(self.connect_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                self.evict_if_full();
                self.connections
                    .push_back(ReadyConnection::new(stream, addr));
                debug!(
                    "[ConnPool] Prewarmed connection to {} (pool={}/{})",
                    addr,
                    self.connections.len(),
                    self.max_idle
                );
                Ok(true)
            }
            Ok(Err(e)) => Err(format!("Connect to {} failed: {}", addr, e)),
            Err(_) => Err(format!(
                "Connect to {} timed out after {:?}",
                addr, self.connect_timeout
            )),
        }
    }

    pub fn try_take(&mut self, addr: &SocketAddr) -> Option<ReadyConnection> {
        let pos = self.connections.iter().position(|c| &c.addr == addr)?;
        let conn = self.connections.remove(pos).unwrap();
        debug!(
            "[ConnPool] Took connection to {} (pool={})",
            addr,
            self.connections.len()
        );
        Some(conn)
    }

    pub fn return_connection(&mut self, conn: ReadyConnection) {
        if conn.is_stale(self.stale_threshold) {
            debug!("[ConnPool] Dropping stale connection to {}", conn.addr);
            return;
        }
        self.evict_if_full();
        self.connections.push_back(conn);
    }

    pub fn contains_addr(&self, addr: &SocketAddr) -> bool {
        self.connections.iter().any(|c| &c.addr == addr)
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    pub async fn evict_stale(&mut self) -> usize {
        let before = self.connections.len();
        self.connections
            .retain(|c| !c.is_stale(self.stale_threshold));
        before - self.connections.len()
    }

    fn evict_if_full(&mut self) {
        while self.connections.len() >= self.max_idle {
            if let Some(old) = self.connections.pop_front() {
                debug!(
                    "[ConnPool] Evicting oldest connection to {} (age={:?})",
                    old.addr,
                    old.age()
                );
            } else {
                break;
            }
        }
    }
}

impl Default for BtConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_starts_empty() {
        let pool = BtConnectionPool::new();
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }

    #[tokio::test]
    async fn test_prewarm_adds_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut pool = BtConnectionPool::new();
        let result = pool.prewarm(addr).await;

        assert!(result.is_ok());
        assert!(result.unwrap());
        assert_eq!(pool.len(), 1);
        assert!(pool.contains_addr(&addr));
    }

    #[tokio::test]
    async fn test_try_take_removes_from_pool() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut pool = BtConnectionPool::new();
        pool.prewarm(addr).await.unwrap();

        let conn = pool.try_take(&addr);
        assert!(conn.is_some());
        assert_eq!(pool.len(), 0);
        assert!(!pool.contains_addr(&addr));

        let taken = conn.unwrap();
        assert_eq!(taken.addr, addr);
    }

    #[tokio::test]
    async fn test_return_connection_readds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut pool = BtConnectionPool::new();
        pool.prewarm(addr).await.unwrap();

        let conn = pool.try_take(&addr).unwrap();
        assert_eq!(pool.len(), 0);

        pool.return_connection(conn);
        assert_eq!(pool.len(), 1);
        assert!(pool.contains_addr(&addr));
    }

    #[tokio::test]
    async fn test_eviction_on_max_idle() {
        let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr1 = listener1.local_addr().unwrap();

        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();

        let mut pool = BtConnectionPool::new().with_max_idle(1);

        pool.prewarm(addr1).await.unwrap();
        assert_eq!(pool.len(), 1);

        pool.prewarm(addr2).await.unwrap();
        assert_eq!(pool.len(), 1);
        assert!(pool.contains_addr(&addr2));
        assert!(!pool.contains_addr(&addr1));
    }

    #[tokio::test]
    async fn test_duplicate_prewarm_skips() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut pool = BtConnectionPool::new();

        let result1 = pool.prewarm(addr).await.unwrap();
        assert!(result1);
        assert_eq!(pool.len(), 1);

        let result2 = pool.prewarm(addr).await.unwrap();
        assert!(!result2);
        assert_eq!(pool.len(), 1);
    }
}
