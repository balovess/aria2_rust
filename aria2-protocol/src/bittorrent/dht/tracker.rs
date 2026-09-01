//! Transaction tracker for matching outbound DHT queries to inbound responses.
//!
//! Unlike the simpler `TransactionManager` which uses `FnOnce` callbacks,
//! this tracker uses `tokio::sync::oneshot` channels so that async lookup
//! tasks can `.await` their responses directly — eliminating the callback
//! hierarchy found in the C++ implementation.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::{debug, trace};

use super::message::DhtMessage;

/// Type of outbound DHT query being tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryType {
    Ping,
    FindNode,
    GetPeers,
    AnnouncePeer,
    SampleInfohashes,
    Get,
    Put,
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

/// A tracked response wait that removes its transaction if the caller drops
/// it before a response arrives.
pub struct TrackedResponseWait {
    transaction_id: [u8; 4],
    receiver: Option<tokio::sync::oneshot::Receiver<TrackedResponse>>,
    tracker: Arc<TransactionTracker>,
}

impl TrackedResponseWait {
    /// Wait for the response, treating timeout and channel closure uniformly
    /// as a missing response.
    pub async fn wait(mut self, timeout: Duration) -> Option<TrackedResponse> {
        let receiver = self.receiver.take()?;
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(response)) => Some(response),
            Ok(Err(_)) | Err(_) => None,
        }
    }
}

impl Drop for TrackedResponseWait {
    fn drop(&mut self) {
        self.tracker.cancel(&self.transaction_id);
    }
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
    change_notify: Arc<Notify>,
}

struct TransactionTrackerInner {
    transactions: HashMap<[u8; 4], PendingTransaction>,
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
            change_notify: Arc::new(Notify::new()),
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
        let (transaction_id, response_rx) = self.allocate_inner(
            query_type,
            target_addr,
            target_id,
            info_hash,
            timeout,
        );
        (transaction_id.to_vec(), response_rx)
    }

    fn allocate_inner(
        &self,
        query_type: QueryType,
        target_addr: SocketAddr,
        target_id: Option<[u8; 20]>,
        info_hash: Option<[u8; 20]>,
        timeout: Duration,
    ) -> ([u8; 4], tokio::sync::oneshot::Receiver<TrackedResponse>) {
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let tx_id = inner.next_tx_id;
        inner.next_tx_id = inner.next_tx_id.wrapping_add(1);
        let key = tx_id.to_be_bytes();

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
        self.change_notify.notify_one();

        (key, response_rx)
    }

    /// Allocate a transaction and return an owned response wait.
    pub fn allocate_wait(
        self: &Arc<Self>,
        query_type: QueryType,
        target_addr: SocketAddr,
        target_id: Option<[u8; 20]>,
        info_hash: Option<[u8; 20]>,
        timeout: Duration,
    ) -> (u32, TrackedResponseWait) {
        let (transaction_id, response_rx) =
            self.allocate_inner(query_type, target_addr, target_id, info_hash, timeout);
        let numeric_id = u32::from_be_bytes(transaction_id);
        (
            numeric_id,
            TrackedResponseWait {
                transaction_id,
                receiver: Some(response_rx),
                tracker: Arc::clone(self),
            },
        )
    }

    /// Cancel a pending transaction and close its response channel.
    pub fn cancel(&self, tx_id: &[u8]) -> bool {
        let Ok(key) = <[u8; 4]>::try_from(tx_id) else {
            return false;
        };
        let removed = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned")
            .transactions
            .remove(&key)
            .is_some();
        if removed {
            self.change_notify.notify_one();
        }
        removed
    }

    /// Match an inbound response to a pending transaction.
    ///
    /// If a matching transaction is found, the response is delivered to the
    /// waiting task via the oneshot channel. Returns `true` if matched.
    pub fn handle_response(&self, tx_id: &[u8], response: DhtMessage, from: SocketAddr) -> bool {
        let Ok(key) = <[u8; 4]>::try_from(tx_id) else {
            return false;
        };
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let Some(pending) = inner.transactions.get(&key) else {
            debug!(
                tx_id = %hex::encode(tx_id),
                from = %from,
                "Received DHT response for unknown transaction"
            );
            return false;
        };
        if pending.target_addr != from {
            debug!(
                tx_id = %hex::encode(tx_id),
                expected = %pending.target_addr,
                actual = %from,
                "Received DHT response from unexpected address"
            );
            return false;
        }

        if let Some(mut pending) = inner.transactions.remove(&key) {
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
            self.change_notify.notify_one();
            true
        } else {
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

    /// Return the time remaining until the next pending transaction expires.
    ///
    /// `None` means there are no transactions and therefore no timeout work
    /// for a receive loop to schedule.
    pub fn next_timeout(&self) -> Option<Duration> {
        let inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let now = Instant::now();
        inner
            .transactions
            .values()
            .map(|pending| {
                let deadline = pending.created_at + pending.timeout;
                deadline.saturating_duration_since(now)
            })
            .min()
    }

    /// Return the notification source for transaction-set changes.
    pub fn change_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.change_notify)
    }

    /// Clean up transactions that exceeded three of their configured timeouts.
    /// This is a safety net in case `handle_timeouts` isn't called.
    pub fn cleanup_expired(&self) -> usize {
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let now = Instant::now();
        let before = inner.transactions.len();
        inner.transactions.retain(|_, pending| {
            let max_age = pending.timeout.saturating_mul(3);
            now.duration_since(pending.created_at) < max_age
        });
        let removed = before - inner.transactions.len();
        if removed > 0 {
            self.change_notify.notify_one();
        }
        removed
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

        let timeout = Duration::from_secs(10);
        let (id1, _) = tracker.allocate(QueryType::Ping, addr, None, None, timeout);
        let (id2, _) = tracker.allocate(QueryType::Ping, addr, None, None, timeout);

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

    #[tokio::test]
    async fn test_response_wait_cancels_when_dropped() {
        let tracker = Arc::new(TransactionTracker::new());
        let addr: SocketAddr = "10.0.0.5:6881".parse().unwrap();

        let (_tx_id, response_wait) =
            tracker.allocate_wait(QueryType::Ping, addr, None, None, Duration::from_secs(10));
        assert_eq!(tracker.pending_count(), 1);

        drop(response_wait);
        assert_eq!(tracker.pending_count(), 0);
    }
}
