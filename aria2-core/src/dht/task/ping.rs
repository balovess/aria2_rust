//! DHT ping task for verifying node connectivity.

use std::time::Duration;

use tracing::{debug, trace};

use super::super::constants::MESSAGE_TIMEOUT_SECS;
use super::super::node::DhtNode;
use super::DhtTask;

/// A DHT ping task to verify node connectivity.
///
/// C++: `DHTPingTask` - sends ping to a remote node with retry support.
pub struct DhtPingTask {
    /// The remote node to ping.
    remote_node: DhtNode,
    /// Maximum number of retries.
    max_retry: u32,
    /// Current retry count.
    retry_count: u32,
    /// Whether the ping was successful.
    success: bool,
    /// Whether the task is finished.
    done: bool,
    /// Timeout for each ping attempt.
    timeout: Duration,
}

impl DhtPingTask {
    /// Create a new ping task for the given remote node.
    pub fn new(remote_node: DhtNode, max_retry: u32) -> Self {
        Self {
            remote_node,
            max_retry,
            retry_count: 0,
            success: false,
            done: false,
            timeout: Duration::from_secs(MESSAGE_TIMEOUT_SECS),
        }
    }

    /// Handle a successful ping response.
    pub fn on_response(&mut self) {
        debug!(addr = %self.remote_node.addr(), "Ping successful");
        self.success = true;
        self.done = true;
    }

    /// Handle a ping timeout - retry if retries remain.
    pub fn on_timeout(&mut self) -> bool {
        self.retry_count += 1;
        if self.retry_count > self.max_retry {
            debug!(
                addr = %self.remote_node.addr(),
                retries = self.retry_count,
                "Ping failed after max retries"
            );
            self.done = true;
            return false; // no more retries
        }
        trace!(
            addr = %self.remote_node.addr(),
            retry = self.retry_count,
            "Ping timeout, retrying"
        );
        true // retry
    }

    /// Get the remote node being pinged.
    pub fn remote_node(&self) -> &DhtNode {
        &self.remote_node
    }

    /// Whether the ping was successful.
    pub fn is_success(&self) -> bool {
        self.success
    }

    /// Get the timeout duration.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl DhtTask for DhtPingTask {
    fn startup(&mut self) {
        // Ping is sent by the message dispatcher when the task is started.
        trace!(addr = %self.remote_node.addr(), "Starting ping task");
    }

    fn finished(&self) -> bool {
        self.done
    }
}
