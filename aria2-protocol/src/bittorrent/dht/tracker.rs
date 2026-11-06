//! Transaction tracker for matching outbound DHT queries to inbound responses.
//!
//! The tracker uses `tokio::sync::oneshot` channels so that async lookup tasks
//! can `.await` their responses directly.

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
    deadline: Instant,
}

impl TrackedResponseWait {
    /// Wait for the response, treating timeout and channel closure uniformly
    /// as a missing response.
    pub async fn wait(mut self) -> Option<TrackedResponse> {
        let receiver = self.receiver.take()?;
        let deadline = tokio::time::Instant::from_std(self.deadline);
        match tokio::time::timeout_at(deadline, receiver).await {
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
    /// Channel to deliver the response to the waiting task.
    response_tx: Option<tokio::sync::oneshot::Sender<TrackedResponse>>,
    /// When this transaction was created (for timeout calculation).
    created_at: Instant,
    /// Absolute response deadline shared with the wait handle.
    deadline: Instant,
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

    /// Allocate a transaction and return an owned response wait.
    pub fn allocate_wait(
        self: &Arc<Self>,
        query_type: QueryType,
        target_addr: SocketAddr,
        timeout: Duration,
    ) -> (u32, TrackedResponseWait) {
        let (transaction_id, response_rx, deadline) = {
            let mut inner = self
                .inner
                .lock()
                .expect("TransactionTracker mutex poisoned");
            let transaction_id = inner.next_tx_id.to_be_bytes();
            inner.next_tx_id = inner.next_tx_id.wrapping_add(1);

            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let created_at = Instant::now();
            let deadline = created_at + timeout;
            inner.transactions.insert(
                transaction_id,
                PendingTransaction {
                    query_type,
                    target_addr,
                    response_tx: Some(response_tx),
                    created_at,
                    deadline,
                },
            );
            (transaction_id, response_rx, deadline)
        };
        self.change_notify.notify_one();

        let numeric_id = u32::from_be_bytes(transaction_id);
        (
            numeric_id,
            TrackedResponseWait {
                transaction_id,
                receiver: Some(response_rx),
                tracker: Arc::clone(self),
                deadline,
            },
        )
    }

    /// Cancel a pending transaction and close its response channel.
    fn cancel(&self, transaction_id: &[u8; 4]) {
        let removed = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned")
            .transactions
            .remove(transaction_id)
            .is_some();
        if removed {
            self.change_notify.notify_one();
        }
    }

    /// Match an inbound response to a pending transaction.
    ///
    /// A matching KRPC response is delivered to the waiting task. A matching
    /// KRPC error closes the waiter so callers follow their normal failure
    /// path. Returns `true` when the transaction was matched.
    pub fn handle_response(&self, response: DhtMessage, from: SocketAddr) -> bool {
        let tx_id = response.t.as_slice();
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
            let is_error = response.is_error();
            trace!(
                tx_id = %hex::encode(tx_id),
                query_type = ?pending.query_type,
                rtt_ms = rtt.as_millis(),
                "Matched DHT response to pending transaction"
            );
            if let Some(tx) = pending.response_tx.take() {
                if is_error {
                    drop(tx);
                } else {
                    let _ = tx.send(TrackedResponse {
                        message: response,
                        from,
                        rtt,
                    });
                }
            }
            self.change_notify.notify_one();
            true
        } else {
            false
        }
    }

    /// Process timed-out transactions.
    ///
    /// Remove expired transactions and close their response channels.
    pub fn handle_timeouts(&self) -> usize {
        let mut inner = self
            .inner
            .lock()
            .expect("TransactionTracker mutex poisoned");
        let now = Instant::now();
        let before = inner.transactions.len();

        inner.transactions.retain(|tx_id, pending| {
            if now >= pending.deadline {
                debug!(
                    tx_id = %hex::encode(tx_id),
                    query_type = ?pending.query_type,
                    target = %pending.target_addr,
                    "DHT transaction timed out"
                );
                // Dropping the oneshot Sender without sending signals RecvError
                // to the waiting Receiver.
                false
            } else {
                true
            }
        });

        before - inner.transactions.len()
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
            .map(|pending| pending.deadline.saturating_duration_since(now))
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
            let max_age = pending
                .deadline
                .saturating_duration_since(pending.created_at)
                .saturating_mul(3);
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

    #[tokio::test]
    async fn test_allocate_and_match() {
        let tracker = Arc::new(TransactionTracker::new());
        let addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();

        let (tx_id, response_wait) =
            tracker.allocate_wait(QueryType::Ping, addr, Duration::from_secs(10));
        let tx_bytes = tx_id.to_be_bytes();

        assert_eq!(tracker.pending_count(), 1);

        // Simulate receiving a response
        let response = DhtMessageBuilder::ping_response(&tx_bytes, &[0xAAu8; 20]);
        assert!(!tracker.handle_response(response.clone(), "10.0.0.2:6881".parse().unwrap()));
        assert_eq!(tracker.pending_count(), 1);
        assert!(tracker.handle_response(response, addr));

        assert_eq!(tracker.pending_count(), 0);

        let tracked = response_wait
            .wait()
            .await
            .expect("tracked response should arrive");
        assert!(tracked.message.is_response());
    }

    #[tokio::test]
    async fn test_error_reply_fails_waiter_and_wrong_source_keeps_transaction() {
        let tracker = Arc::new(TransactionTracker::new());
        let addr: SocketAddr = "10.0.0.7:6881".parse().unwrap();
        let wrong_addr: SocketAddr = "10.0.0.8:6881".parse().unwrap();
        let (tx_id, response_wait) =
            tracker.allocate_wait(QueryType::Ping, addr, Duration::from_secs(10));
        let error = DhtMessageBuilder::error_response(&tx_id.to_be_bytes(), 203, "Protocol Error");

        assert!(!tracker.handle_response(error.clone(), wrong_addr));
        assert_eq!(tracker.pending_count(), 1);
        assert!(tracker.handle_response(error, addr));
        assert_eq!(tracker.pending_count(), 0);
        assert!(response_wait.wait().await.is_none());
    }

    #[test]
    fn test_unknown_transaction_ignored() {
        let tracker = TransactionTracker::new();
        let response = DhtMessageBuilder::ping_response(&[0, 0, 0, 99], &[0u8; 20]);
        assert!(!tracker.handle_response(response, "10.0.0.1:6881".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_handle_timeouts_closes_waiter() {
        let tracker = Arc::new(TransactionTracker::new());
        let addr: SocketAddr = "10.0.0.2:6881".parse().unwrap();

        // Allocate with zero timeout so it's immediately expired
        let (_tx_id, response_wait) =
            tracker.allocate_wait(QueryType::FindNode, addr, Duration::ZERO);

        let timed_out = tracker.handle_timeouts();
        assert_eq!(timed_out, 1);
        assert_eq!(tracker.pending_count(), 0);
        assert!(response_wait.wait().await.is_none());
    }

    #[test]
    fn test_unique_transaction_ids() {
        let tracker = Arc::new(TransactionTracker::new());
        let addr: SocketAddr = "10.0.0.3:6881".parse().unwrap();

        let timeout = Duration::from_secs(10);
        let (id1, _wait1) = tracker.allocate_wait(QueryType::Ping, addr, timeout);
        let (id2, _wait2) = tracker.allocate_wait(QueryType::Ping, addr, timeout);

        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn test_response_wait_cancels_when_dropped() {
        let tracker = Arc::new(TransactionTracker::new());
        let addr: SocketAddr = "10.0.0.5:6881".parse().unwrap();

        let (_tx_id, response_wait) =
            tracker.allocate_wait(QueryType::Ping, addr, Duration::from_secs(10));
        assert_eq!(tracker.pending_count(), 1);

        drop(response_wait);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_response_wait_deadline_cleans_transaction_without_engine_timeout_loop() {
        let tracker = Arc::new(TransactionTracker::new());
        let addr: SocketAddr = "10.0.0.6:6881".parse().unwrap();
        let (_tx_id, response_wait) = tracker.allocate_wait(QueryType::Ping, addr, Duration::ZERO);

        assert!(response_wait.wait().await.is_none());
        assert_eq!(tracker.pending_count(), 0);
    }
}
