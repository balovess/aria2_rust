//! DHT announce task for sending announce_peer messages.

use std::net::SocketAddr;

use tracing::trace;

use super::super::node_id::NodeId;
use super::DhtTask;

/// A DHT announce task that sends `announce_peer` messages to nodes
/// that provided tokens during a peer lookup.
///
/// C++: This functionality was embedded in `DHTPeerLookupTask::onFinish()`
/// which called `DHTPeerAnnounceStorage::announcePeer()`. In Rust, this
/// is extracted into a separate task for cleaner separation of concerns.
///
/// After a peer lookup completes, the discovered tokens are used to
/// send `announce_peer` queries to the nodes that returned them,
/// informing the DHT network that this client has the requested info hash.
pub struct DhtAnnounceTask {
    /// The info hash being announced.
    info_hash: NodeId,
    /// Nodes to announce to, paired with their tokens.
    /// Each entry is (node_addr, token).
    announce_targets: Vec<(SocketAddr, Vec<u8>)>,
    /// Index of the next target to announce to.
    next_target: usize,
    /// Whether the task has started.
    started: bool,
    /// Whether the task is finished.
    done: bool,
}

impl DhtAnnounceTask {
    /// Create a new announce task for the given info hash with discovered tokens.
    ///
    /// `tokens` comes from a completed `DhtLookupTask`'s `LookupResult::tokens`.
    pub fn new(info_hash: NodeId, tokens: Vec<(SocketAddr, Vec<u8>)>) -> Self {
        Self {
            info_hash,
            announce_targets: tokens,
            next_target: 0,
            started: false,
            done: false,
        }
    }

    /// Get the info hash being announced.
    pub fn info_hash(&self) -> &NodeId {
        &self.info_hash
    }

    /// Get the next announce target (addr, token), if any remain.
    pub fn next_announce_target(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        if self.next_target >= self.announce_targets.len() {
            return None;
        }
        let target = self.announce_targets[self.next_target].clone();
        self.next_target += 1;

        // All targets have been consumed - task is done
        if self.next_target >= self.announce_targets.len() {
            self.done = true;
        }

        Some(target)
    }

    /// Total number of announce targets.
    #[allow(dead_code)]
    pub fn total_targets(&self) -> usize {
        self.announce_targets.len()
    }

    /// Number of remaining announce targets.
    #[allow(dead_code)]
    pub fn remaining_targets(&self) -> usize {
        self.announce_targets.len().saturating_sub(self.next_target)
    }
}

impl DhtTask for DhtAnnounceTask {
    fn startup(&mut self) {
        self.started = true;
        trace!(
            info_hash = %self.info_hash,
            targets = self.announce_targets.len(),
            "Starting DHT announce task"
        );
        // If no targets, finish immediately
        if self.announce_targets.is_empty() {
            self.done = true;
        }
    }

    fn finished(&self) -> bool {
        self.done
    }
}
