use std::time::Duration;

/// Maximum number of distinct local connection IDs representable on a uTP
/// socket. Connection ID zero is valid, so the full `u16` space is usable.
const MAX_CONNECTIONS: usize = u16::MAX as usize + 1;

/// Configuration for one uTP socket.
///
/// These settings govern socket resource limits and connection lifecycle. The
/// retransmission and congestion-control algorithms remain internal protocol
/// policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtpSocketOptions {
    max_connections: usize,
    connect_timeout: Duration,
    idle_timeout: Duration,
    keepalive_interval: Duration,
}

impl UtpSocketOptions {
    /// Set the maximum simultaneous incoming and outgoing connections.
    /// Values are clamped to the range supported by the 16-bit connection ID.
    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections.clamp(1, MAX_CONNECTIONS);
        self
    }

    /// Set the maximum time allowed to establish a connection.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set how long an inactive connection may remain open.
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Set the interval between keep-alive packets.
    pub fn with_keepalive_interval(mut self, interval: Duration) -> Self {
        self.keepalive_interval = interval;
        self
    }

    pub(crate) fn max_connections(&self) -> usize {
        self.max_connections
    }

    pub(crate) fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub(crate) fn set_connect_timeout(&mut self, timeout: Duration) {
        self.connect_timeout = timeout;
    }

    pub(crate) fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    pub(crate) fn set_idle_timeout(&mut self, timeout: Duration) {
        self.idle_timeout = timeout;
    }

    pub(crate) fn keepalive_interval(&self) -> Duration {
        self.keepalive_interval
    }

    pub(crate) fn set_keepalive_interval(&mut self, interval: Duration) {
        self.keepalive_interval = interval;
    }
}

impl Default for UtpSocketOptions {
    fn default() -> Self {
        Self {
            max_connections: 100,
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(15),
        }
    }
}
