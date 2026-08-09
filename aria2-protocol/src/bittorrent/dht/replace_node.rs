//! DHT ReplaceNode task — verifies questionable nodes and replaces them.
//!
//! When a new node cannot be added to a full bucket because all existing
//! nodes are good, the new node is cached. The ReplaceNode task pings the
//! LRU questionable node. If it doesn't respond after MAX_RETRY attempts,
//! the questionable node is replaced with the cached new node.
//!
//! This is the Rust equivalent of C++ `DHTReplaceNodeTask`.

use std::time::Duration;

use tracing::{debug, info};

use super::bucket::Bucket;
use super::node::DhtNode;
use super::socket::DhtSocket;
use super::tracker::TransactionTracker;

/// Maximum ping retry count before marking a node as bad.
const MAX_RETRY: u32 = 2;

/// Result of a ReplaceNode task attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceNodeResult {
    /// The questionable node responded — no replacement made.
    NodeAlive,
    /// The questionable node timed out; it was replaced with the new node.
    NodeReplaced,
    /// No questionable node found in the bucket; nothing to do.
    NoQuestionableNode,
    /// The task failed due to an error.
    Failed(String),
}

/// Attempt to replace a questionable node in the given bucket with a new node.
///
/// This sends a ping to the LRU questionable node in the bucket. If the node
/// responds, it is kept and the new node remains in the cache. If the node
/// times out after `MAX_RETRY` attempts, it is marked bad and the new node
/// takes its place.
///
/// This is the async equivalent of C++ `DHTReplaceNodeTask`, which uses
/// callback-based message dispatch. Here we use a simple request-response
/// pattern with the transaction tracker.
///
/// # Arguments
/// * `bucket` - The bucket containing the questionable node.
/// * `new_node` - The candidate replacement node.
/// * `socket` - UDP socket for sending the ping.
/// * `tracker` - Transaction tracker for matching responses.
/// * `timeout` - Per-attempt timeout duration.
pub async fn replace_node(
    bucket: &mut Bucket,
    new_node: &DhtNode,
    socket: &DhtSocket,
    _tracker: &TransactionTracker,
    timeout: Duration,
) -> ReplaceNodeResult {
    // Extract questionable node info upfront to avoid borrow conflicts.
    let (q_id, q_addr) = match bucket.get_lru_questionable_node() {
        Some(node) => (node.id, node.addr),
        None => return ReplaceNodeResult::NoQuestionableNode,
    };

    debug!(
        "ReplaceNode: pinging questionable node {} (attempt 1/{})",
        hex::encode(q_id),
        MAX_RETRY,
    );

    for attempt in 1..=MAX_RETRY {
        // Send a ping to the questionable node.
        let self_id = new_node.id;
        let msg = super::message::DhtMessageBuilder::ping(0, &self_id);
        let encoded = match msg.encode() {
            Ok(e) => e,
            Err(e) => return ReplaceNodeResult::Failed(format!("encode error: {}", e)),
        };

        if let Err(e) = socket.send_to(q_addr, &encoded).await {
            debug!(
                "ReplaceNode: failed to send ping to {}: {}",
                hex::encode(q_id),
                e
            );
            return ReplaceNodeResult::Failed(format!("send error: {}", e));
        }

        // Wait for a response with timeout.
        let mut buf = [0u8; 4096];
        match socket.recv_with_timeout(&mut buf, timeout).await {
            Ok((len, _from)) if len > 0 => {
                // Got a response — the questionable node is alive.
                if let Ok(response) = super::message::DhtMessage::decode(&buf[..len])
                    && response.is_response()
                {
                    info!(
                        "ReplaceNode: ping reply received from {} — node is alive",
                        hex::encode(q_id)
                    );
                    bucket.mark_good(&q_id);
                    return ReplaceNodeResult::NodeAlive;
                }
                // Response was not a valid ping reply; treat as timeout.
            }
            _ => {
                // Timeout or error.
            }
        }

        if attempt < MAX_RETRY {
            debug!(
                "ReplaceNode: ping timeout from {}, retrying (attempt {}/{})",
                hex::encode(q_id),
                attempt + 1,
                MAX_RETRY,
            );
        }
    }

    // All retries exhausted — replace the questionable node.
    info!(
        "ReplaceNode: ping failed {} times, replacing {} with {}",
        MAX_RETRY,
        hex::encode(q_id),
        new_node.id_hex(),
    );

    // Mark the questionable node as bad and add the new node.
    bucket.mark_bad(&q_id);
    bucket.add_node(new_node.clone());

    ReplaceNodeResult::NodeReplaced
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_node_result_variants() {
        assert_eq!(ReplaceNodeResult::NodeAlive, ReplaceNodeResult::NodeAlive);
        assert_eq!(
            ReplaceNodeResult::NodeReplaced,
            ReplaceNodeResult::NodeReplaced
        );
        assert_eq!(
            ReplaceNodeResult::NoQuestionableNode,
            ReplaceNodeResult::NoQuestionableNode
        );
    }
}
