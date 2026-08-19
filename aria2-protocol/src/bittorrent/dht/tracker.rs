//! Transaction tracker for matching outbound DHT queries to inbound responses.
//!
//! Unlike the simpler `TransactionManager` which uses `FnOnce` callbacks,
//! this tracker uses `tokio::sync::oneshot` channels so that async lookup
//! tasks can `.await` their responses directly — eliminating the callback
//! hierarchy found in the C++ implementation.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tracing::{debug, trace};

use super::message::DhtMessage;

/// Default timeout for a single DHT query before considering it lost.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Type of outbound DHT query being tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryType {
    Ping,
    FindNode,
    GetPeers,
    AnnouncePeer,
}

/// Response delivered to the waiting task via oneshot channel.
#[derive(Debug)]
pub struct TrackedResponse {
    /// The decoded KRPC response message.
    pub message: DhtMessage,
    /// The remote node that sent the response.
    pub from: SocketAddr,
    /// Round-trip time from query send to response receipt.
    pub rtt: Duration,
}

/// A pending outbound transaction awaiting a response.
struct PendingTransaction {
    /// Type of the original query.
    query_type: QueryType,
    /// Target node address the query was sent to.
    target_addr: SocketAddr,
    /// Target node ID (if known at send time).
    target_id: Option<[u8; 20]>,
    /// Info hash for get_peers / announce_peer queries.
    #[allow(dead_code)]
    info_hash: Option<[u8; 20]>,
    /// Token received from get_peers response (for subsequent announce).
    #[allow(dead_code)]
    token: Option<Vec<u8>>,
    /// Channel to deliver the response to the waiting task.
    response_tx: Option<tokio::sync::oneshot::Sender<TrackedResponse>>,
    /// When this transaction was created (for timeout calculation).
    created_at: Instant,
    /// Query timeout duration.
    timeout: Duration,
}

/// Tracks outbound DHT query transactions and matches inbound responses.
///
/// Thread-safe via an internal `std::sync::Mutex` — operations are brief
/// and never span `.await` points.
pub struct TransactionTracker {
    inner: std::sync::Mutex<TransactionTrackerInner>,
}

struct TransactionTrackerInner {
    transactions: HashMap<Vec<u8>, PendingTransaction>,
    next_tx_id: u32,
}

impl TransactionTracker {
    /// Create a new empty tracker.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(TransactionTrackerInner {
                transactions: HashMap::new(),
                next_tx_id: 1,
            }),
        }
    }

    /// Allocate a new transaction ID for an outbound query.
    ///
    /// Returns the transaction ID bytes and a `oneshot::Receiver` that will
    /// receive the response when it arrives (or be closed on timeout).
    pub fn allocate(
        &self,
        query_type: QueryType,
        target_addr: SocketAddr,
        target_id: Option<[u8; 20]>,
        info_hash: Option<[u8; 20]>,
        timeout: Duration,
    ) -> (Vec<u8>, tokio::sync::oneshot::Receiver<TrackedResponse>) {
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let tx_id = inner.next_tx_id;
        inner.next_tx_id = inner.next_tx_id.wrapping_add(1);
        let key = tx_id.to_be_bytes().to_vec();

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        inner.transactions.insert(
            key.clone(),
            PendingTransaction {
                query_type,
                target_addr,
                target_id,
                info_hash,
                token: None,
                response_tx: Some(response_tx),
                created_at: Instant::now(),
                timeout,
            },
        );

        (key, response_rx)
    }

    /// Match an inbound response to a pending transaction.
    ///
    /// If a matching transaction is found, the response is delivered to the
    /// waiting task via the oneshot channel. Returns `true` if matched.
    pub fn handle_response(&self, tx_id: &[u8], response: DhtMessage, from: SocketAddr) -> bool {
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        if let Some(mut pending) = inner.transactions.remove(tx_id) {
            let rtt = pending.created_at.elapsed();
            trace!(
                tx_id = %hex::encode(tx_id),
                query_type = ?pending.query_type,
                rtt_ms = rtt.as_millis(),
                "Matched DHT response to pending transaction"
            );
            if let Some(tx) = pending.response_tx.take() {
                let _ = tx.send(TrackedResponse {
                    message: response,
                    from,
                    rtt,
                });
            }
            true
        } else {
            debug!(
                tx_id = %hex::encode(tx_id),
                from = %from,
                "Received DHT response for unknown transaction"
            );
            false
        }
    }

    /// Store a token received in a get_peers response for later announce.
    ///
    /// This must be called after `handle_response` matches a get_peers reply,
    /// before the caller sends an announce_peer to the same node.
    pub fn store_token_for_node(
        &self,
        node_addr: &SocketAddr,
        info_hash: &[u8; 20],
        token: Vec<u8>,
    ) {
        // Tokens are stored externally by the lookup task in a simple HashMap.
        // This method is a no-op placeholder — the lookup task manages tokens
        // directly to avoid coupling the tracker to per-info-hash state.
        let _ = (node_addr, info_hash, token);
    }

    /// Process timed-out transactions.
    ///
    /// Removes all transactions whose timeout has elapsed and closes their
    /// oneshot channels (which signals `RecvError` to the waiting task).
    /// Returns a list of (target_addr, query_type, target_id) for each
    /// timed-out transaction, so the caller can mark nodes as failed.
    pub fn handle_timeouts(&self) -> Vec<(SocketAddr, QueryType, Option<[u8; 20]>)> {
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let now = Instant::now();
        let mut timed_out = Vec::new();

        inner.transactions.retain(|tx_id, pending| {
            if now.duration_since(pending.created_at) >= pending.timeout {
                debug!(
                    tx_id = %hex::encode(tx_id),
                    query_type = ?pending.query_type,
                    target = %pending.target_addr,
                    "DHT transaction timed out"
                );
                timed_out.push((pending.target_addr, pending.query_type, pending.target_id));
                // Dropping the oneshot Sender without sending signals RecvError
                // to the waiting Receiver.
                false
            } else {
                true
            }
        });

        timed_out
    }

    /// Number of currently pending transactions.
    pub fn pending_count(&self) -> usize {
        let inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        inner.transactions.len()
    }

    /// Clean up expired transactions (older than `QUERY_TIMEOUT * 3`).
    /// This is a safety net in case `handle_timeouts` isn't called.
    pub fn cleanup_expired(&self) -> usize {
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let now = Instant::now();
        let max_age = QUERY_TIMEOUT * 3;
        let before = inner.transactions.len();
        inner
            .transactions
            .retain(|_, pending| now.duration_since(pending.created_at) < max_age);
        before - inner.transactions.len()
    }
}

impl Default for TransactionTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bittorrent::dht::message::DhtMessageBuilder;

    #[test]
    fn test_allocate_and_match() {
        let tracker = TransactionTracker::new();
        let addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();

        let (tx_id, mut rx) =
            tracker.allocate(QueryType::Ping, addr, None, None, Duration::from_secs(10));

        assert_eq!(tracker.pending_count(), 1);

        // Simulate receiving a response
        let response = DhtMessageBuilder::ping_response(&tx_id, &[0xAAu8; 20]);
        assert!(tracker.handle_response(&tx_id, response, addr));

        assert_eq!(tracker.pending_count(), 0);

        // The oneshot receiver should have the response
        let tracked = rx.try_recv().unwrap();
        assert!(tracked.message.is_response());
    }

    #[test]
    fn test_unknown_transaction_ignored() {
        let tracker = TransactionTracker::new();
        let response = DhtMessageBuilder::ping_response(&[0, 0, 0, 99], &[0u8; 20]);
        assert!(!tracker.handle_response(
            &[0, 0, 0, 99],
            response,
            "10.0.0.1:6881".parse().unwrap()
        ));
    }

    #[test]
    fn test_handle_timeouts() {
        let tracker = TransactionTracker::new();
        let addr: SocketAddr = "10.0.0.2:6881".parse().unwrap();

        // Allocate with zero timeout so it's immediately expired
        let (_tx_id, _rx) = tracker.allocate(
            QueryType::FindNode,
            addr,
            Some([1u8; 20]),
            None,
            Duration::ZERO,
        );

        // Small sleep to ensure time has passed
        std::thread::sleep(std::time::Duration::from_millis(1));

        let timed_out = tracker.handle_timeouts();
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0].0, addr);
        assert_eq!(timed_out[0].1, QueryType::FindNode);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_unique_transaction_ids() {
        let tracker = TransactionTracker::new();
        let addr: SocketAddr = "10.0.0.3:6881".parse().unwrap();

        let (id1, _) = tracker.allocate(QueryType::Ping, addr, None, None, QUERY_TIMEOUT);
        let (id2, _) = tracker.allocate(QueryType::Ping, addr, None, None, QUERY_TIMEOUT);

        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn test_oneshot_closed_on_timeout() {
        let tracker = TransactionTracker::new();
        let addr: SocketAddr = "10.0.0.4:6881".parse().unwrap();

        let (_tx_id, mut rx) = tracker.allocate(
            QueryType::GetPeers,
            addr,
            None,
            Some([2u8; 20]),
            Duration::ZERO,
        );

        std::thread::sleep(std::time::Duration::from_millis(1));
        tracker.handle_timeouts();

        // The oneshot should be closed (sender dropped without sending)
        use tokio::sync::oneshot::error::TryRecvError;
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Closed)));
    }
}
