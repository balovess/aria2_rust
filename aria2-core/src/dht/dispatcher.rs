//! DHT message dispatcher for outbound queries.
//!
//! Maintains a queue of outbound DHT messages, tracks them via the
//! [`DhtMessageTracker`], and sends them over the UDP transport.
//!
//! C++ reference: `DHTMessageDispatcher.h/cc` + `DHTMessageDispatcherImpl.h/cc`

use std::net::SocketAddr;
use std::time::Duration;

use tracing::{trace, warn};

use super::constants::MESSAGE_TIMEOUT_SECS;
use super::message::DhtMessage;
use super::message_codec;
use super::node_id::NodeId;
use super::tracker::{DhtMessageTracker, MatchResult, TimeoutEntry};
use super::transport::DhtTransport;

// ── Queued message ────────────────────────────────────────────────────────

/// An outbound DHT message waiting to be sent.
struct QueuedMessage {
    /// The DHT message to send.
    message: DhtMessage,
    /// Timeout for tracking this message (passed to the tracker on send).
    timeout: Duration,
}

impl QueuedMessage {
    fn new(message: DhtMessage, timeout: Duration) -> Self {
        Self { message, timeout }
    }
}

// ── DhtDispatcher ─────────────────────────────────────────────────────────

/// Dispatches outbound DHT messages over UDP.
///
/// Messages are first added to an outbound queue. When [`send_messages`] is
/// called, each message is encoded via [`message_codec::encode`] and sent
/// through the transport. The message is then tracked in the
/// [`DhtMessageTracker`] so that inbound responses can be matched.
///
/// C++: `DHTMessageDispatcherImpl`
pub struct DhtDispatcher {
    /// Outbound message queue.
    queue: Vec<QueuedMessage>,
    /// Tracker for matching responses to queries.
    tracker: DhtMessageTracker,
    /// Default timeout for messages without an explicit timeout.
    default_timeout: Duration,
}

impl DhtDispatcher {
    /// Create a new dispatcher with the default message timeout.
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            tracker: DhtMessageTracker::new(),
            default_timeout: Duration::from_secs(MESSAGE_TIMEOUT_SECS),
        }
    }

    /// Create a new dispatcher with a custom default timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            queue: Vec::new(),
            tracker: DhtMessageTracker::with_timeout(timeout),
            default_timeout: timeout,
        }
    }

    /// Add a message to the outbound queue with the default timeout.
    pub fn add_message(&mut self, message: DhtMessage) {
        let timeout = self.default_timeout;
        self.queue.push(QueuedMessage::new(message, timeout));
    }

    /// Add a message to the outbound queue with a custom timeout.
    pub fn add_message_with_timeout(&mut self, message: DhtMessage, timeout: Duration) {
        self.queue.push(QueuedMessage::new(message, timeout));
    }

    /// Send all queued messages over the transport.
    ///
    /// For each message:
    /// 1. Encode the message via [`message_codec::encode`]
    /// 2. Send the encoded bytes via the transport
    /// 3. Track the message in the message tracker
    ///
    /// Messages that fail to encode or send are logged and dropped.
    /// Successfully sent messages are removed from the queue.
    pub async fn send_messages(&mut self, transport: &DhtTransport) {
        let messages = std::mem::take(&mut self.queue);

        for queued in messages {
            let target_addr = *queued.message.sender_addr();
            let target_node_id = queued.message.sender_id().copied().unwrap_or(NodeId::ZERO);
            let method = queued.message.method_name().unwrap_or("unknown").to_owned();
            let transaction_id = queued.message.transaction_id().to_vec();

            let encoded = message_codec::encode(&queued.message);
            match transport.send_message(&encoded, target_addr).await {
                Ok(len) => {
                    trace!(
                        tid = ?transaction_id,
                        addr = %target_addr,
                        method = %method,
                        bytes = len,
                        "Sent DHT message"
                    );
                    // Track the message for response matching with its timeout
                    self.tracker.add_query_with_timeout(
                        target_node_id,
                        target_addr,
                        transaction_id,
                        method,
                        queued.timeout,
                    );
                }
                Err(e) => {
                    warn!(
                        addr = %target_addr,
                        method = %method,
                        error = %e,
                        "Failed to send DHT message"
                    );
                }
            }
        }
    }

    /// Match an inbound response to its original query.
    ///
    /// Returns the match result containing the method name and target node
    /// info. The caller uses this to route the response appropriately.
    pub fn message_arrived(
        &mut self,
        transaction_id: &[u8],
        sender_addr: SocketAddr,
    ) -> Option<MatchResult> {
        self.tracker.match_response(transaction_id, &sender_addr)
    }

    /// Match an inbound response and return the tracked entry.
    ///
    /// This removes the entry from the tracker. The caller uses the method
    /// name to determine how to interpret the response.
    pub fn take_tracked(
        &mut self,
        transaction_id: &[u8],
        sender_addr: SocketAddr,
    ) -> Option<MatchResult> {
        self.tracker.match_response(transaction_id, &sender_addr)
    }

    /// Handle timed-out messages in the tracker.
    ///
    /// Returns a list of all timed-out entries. The caller is responsible
    /// for any timeout side-effects (marking nodes as bad, etc.).
    pub fn handle_timeouts(&mut self) -> Vec<TimeoutEntry> {
        self.tracker.handle_timeout()
    }

    /// Get the number of messages currently in the outbound queue.
    pub fn queue_length(&self) -> usize {
        self.queue.len()
    }

    /// Get the number of tracked (in-flight) messages.
    pub fn tracked_count(&self) -> usize {
        self.tracker.count()
    }
}

impl Default for DhtDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::message::PingQueryPayload;
    use super::super::node_id::NodeId;
    use super::*;

    fn test_addr() -> SocketAddr {
        "192.168.0.1:6881".parse().unwrap()
    }

    #[test]
    fn new_dispatcher_is_empty() {
        let d = DhtDispatcher::new();
        assert_eq!(d.queue_length(), 0);
        assert_eq!(d.tracked_count(), 0);
    }

    #[test]
    fn add_message_increments_queue() {
        let mut d = DhtDispatcher::new();
        let msg = DhtMessage::PingQuery {
            transaction_id: vec![0x01],
            sender_id: NodeId::ZERO,
            sender_addr: test_addr(),
            payload: PingQueryPayload,
        };
        d.add_message(msg);
        assert_eq!(d.queue_length(), 1);
    }

    #[test]
    fn add_multiple_messages() {
        let mut d = DhtDispatcher::new();
        for i in 0..5 {
            let msg = DhtMessage::PingQuery {
                transaction_id: vec![i],
                sender_id: NodeId::ZERO,
                sender_addr: test_addr(),
                payload: PingQueryPayload,
            };
            d.add_message(msg);
        }
        assert_eq!(d.queue_length(), 5);
    }

    #[test]
    fn tracked_after_send() {
        let mut d = DhtDispatcher::new();
        let tid = vec![0x01, 0x02];
        let addr = test_addr();

        // Manually track a message (simulating what send_messages would do)
        d.tracker
            .add_query(NodeId::ZERO, addr, tid.clone(), "ping".to_owned());
        assert_eq!(d.tracked_count(), 1);

        // Match it
        let result = d.take_tracked(&tid, addr);
        assert!(result.is_some());
        assert_eq!(result.unwrap().method, "ping");
        assert_eq!(d.tracked_count(), 0);
    }

    #[test]
    fn handle_timeouts_returns_expired() {
        let mut d = DhtDispatcher::new();
        let short = Duration::from_millis(1);
        d.tracker.add_query_with_timeout(
            NodeId::ZERO,
            test_addr(),
            vec![0x01],
            "ping".to_owned(),
            short,
        );
        d.tracker.add_query_with_timeout(
            NodeId::ZERO,
            test_addr(),
            vec![0x02],
            "ping".to_owned(),
            Duration::from_secs(300),
        );

        std::thread::sleep(Duration::from_millis(5));
        let timed_out = d.handle_timeouts();
        assert_eq!(timed_out.len(), 1);
        assert_eq!(d.tracked_count(), 1);
    }
}
