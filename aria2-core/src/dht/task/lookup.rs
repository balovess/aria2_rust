//! Kademlia iterative lookup algorithm and lookup task wrapper.

use std::collections::VecDeque;
use std::net::SocketAddr;

use tracing::trace;

use super::super::constants::K;
use super::super::node::DhtNode;
use super::super::node_id::NodeId;
use super::super::routing_table::RoutingTable;
use super::{DhtTask, LookupEntry};

/// Kademlia alpha - maximum concurrent in-flight queries per lookup.
const ALPHA: usize = 3;

// -- Lookup kind --------------------------------------------------------------

/// Discriminant for the two lookup variants.
///
/// C++: `DHTNodeLookupTask` vs `DHTPeerLookupTask` (separate classes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupKind {
    /// Find the K closest nodes to a target ID (find_node query).
    Node,
    /// Find peers for an info hash (get_peers query).
    Peer,
}

// -- Lookup result ------------------------------------------------------------

/// Outcome of a completed lookup task.
#[derive(Clone, Debug)]
pub struct LookupResult {
    /// The kind of lookup that was performed.
    pub kind: LookupKind,
    /// The target ID that was looked up.
    pub target: NodeId,
    /// Nodes discovered during the lookup (up to K closest).
    pub nodes: Vec<DhtNode>,
    /// Peers discovered (only for [`LookupKind::Peer`]).
    pub peers: Vec<SocketAddr>,
    /// Tokens received from get_peers responses, keyed by node address.
    /// Used for subsequent announce_peer messages.
    pub tokens: Vec<(SocketAddr, Vec<u8>)>,
}

// -- Lookup state -------------------------------------------------------------

/// Shared state for the Kademlia iterative lookup algorithm.
///
/// C++: `DHTAbstractNodeLookupTask<Resp>` - the core lookup engine.
///
/// The algorithm sends up to `ALPHA` queries concurrently to the closest
/// known nodes. As responses arrive, newly discovered nodes are inserted
/// (sorted by XOR distance to the target), and the next closest unqueried
/// node is sent a query. The lookup terminates when all K closest nodes
/// have been queried and all in-flight messages have completed.
pub struct LookupState {
    /// The target node ID or info hash being looked up.
    pub(super) target: NodeId,
    /// The kind of lookup (node vs peer).
    pub(super) kind: LookupKind,
    /// Candidate nodes sorted by distance to target, closest first.
    pub(super) entries: VecDeque<LookupEntry>,
    /// Number of in-flight messages awaiting a response.
    pub(super) in_flight: usize,
    /// Accumulated discovered nodes (deduped, up to K).
    pub(super) discovered_nodes: Vec<DhtNode>,
    /// Accumulated discovered peers (for peer lookup only).
    pub(super) discovered_peers: Vec<SocketAddr>,
    /// Tokens received from get_peers responses.
    pub(super) tokens: Vec<(SocketAddr, Vec<u8>)>,
    /// Whether the lookup has finished.
    pub(super) done: bool,
}

impl LookupState {
    /// Create a new lookup state for the given target.
    pub fn new(target: NodeId, kind: LookupKind) -> Self {
        Self {
            target,
            kind,
            entries: VecDeque::new(),
            in_flight: 0,
            discovered_nodes: Vec::new(),
            discovered_peers: Vec::new(),
            tokens: Vec::new(),
            done: false,
        }
    }

    /// Seed the lookup with the K closest nodes from the routing table.
    pub fn startup(&mut self, routing_table: &RoutingTable, local_id: &NodeId) {
        let closest = routing_table.get_closest_k_nodes(&self.target);
        for node in closest {
            // Skip the local node
            if node.id() == local_id {
                continue;
            }
            self.entries.push_back(LookupEntry {
                node: node.clone(),
                used: false,
            });
        }

        if self.entries.is_empty() {
            trace!("No seed nodes for lookup, finishing immediately");
            self.done = true;
        }
    }

    /// Get the next batch of nodes to query (up to ALPHA unused entries).
    ///
    /// Returns a list of `(entry_index, node)` pairs. The caller must
    /// mark these entries as used after sending the messages.
    pub fn next_query_batch(&self) -> Vec<(usize, &DhtNode)> {
        let mut batch = Vec::with_capacity(ALPHA);
        let remaining = ALPHA.saturating_sub(self.in_flight);
        for (i, entry) in self.entries.iter().enumerate() {
            if batch.len() >= remaining {
                break;
            }
            if !entry.used {
                batch.push((i, &entry.node));
            }
        }
        batch
    }

    /// Mark entries at the given indices as used (queries sent).
    pub fn mark_sent(&mut self, indices: &[usize]) {
        for &i in indices {
            if let Some(entry) = self.entries.get_mut(i) {
                entry.used = true;
                self.in_flight += 1;
            }
        }
    }

    /// Handle a response from a node in the lookup.
    ///
    /// `sender_addr` identifies which entry responded.
    /// `nodes` are the nodes reported in the response.
    /// `peers` are the peers reported (for peer lookups).
    /// `token` is the announce token (for peer lookups).
    pub fn on_response(
        &mut self,
        sender_addr: SocketAddr,
        nodes: Vec<DhtNode>,
        peers: Vec<SocketAddr>,
        token: Option<Vec<u8>>,
        local_id: &NodeId,
    ) {
        self.in_flight = self.in_flight.saturating_sub(1);

        // Update the responding node's address if it changed
        for entry in &mut self.entries {
            if entry.node.addr() == sender_addr {
                // Node responded successfully - mark as reachable
                entry.node.mark_contacted();
            }
        }

        // Store token from get_peers response
        if let Some(tok) = token {
            self.tokens.push((sender_addr, tok));
        }

        // Add discovered peers
        self.discovered_peers.extend(peers);

        // Insert newly discovered nodes, sorted by distance
        for node in nodes {
            // Skip the local node
            if node.id() == local_id {
                continue;
            }

            // Skip duplicates
            if self.discovered_nodes.iter().any(|n| n.id() == node.id()) {
                continue;
            }
            if self.entries.iter().any(|e| e.node.id() == node.id()) {
                continue;
            }

            self.discovered_nodes.push(node.clone());

            // Insert into entries sorted by distance to target
            let dist = node.id().distance_to(&self.target);
            let pos = self.entries.iter().position(|e| {
                let e_dist = e.node.id().distance_to(&self.target);
                dist < e_dist
            });

            let entry = LookupEntry { node, used: false };

            match pos {
                Some(idx) => self.entries.insert(idx, entry),
                None => self.entries.push_back(entry),
            }
        }

        // Trim to K entries
        while self.entries.len() > K {
            self.entries.pop_back();
        }

        // Deduplicate entries by node ID
        let mut seen = std::collections::HashSet::new();
        self.entries
            .retain(|e| seen.insert(*e.node.id().as_bytes()));

        self.check_finished();
    }

    /// Handle a timeout for a node in the lookup.
    pub fn on_timeout(&mut self, timed_out_addr: SocketAddr) {
        self.in_flight = self.in_flight.saturating_sub(1);

        // Remove the timed-out entry
        self.entries.retain(|e| e.node.addr() != timed_out_addr);

        self.check_finished();
    }

    /// Check if the lookup is complete.
    fn check_finished(&mut self) {
        // Try to send more queries
        let remaining = ALPHA.saturating_sub(self.in_flight);
        let has_unused = self.entries.iter().any(|e| !e.used);

        if remaining > 0 && has_unused {
            // More queries to send - not done yet
            return;
        }

        if self.in_flight == 0 {
            trace!(
                target = %self.target,
                kind = ?self.kind,
                "Lookup finished"
            );
            self.done = true;
        }
    }

    /// Whether the lookup has completed.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Consume the state and produce a result.
    pub fn into_result(self) -> LookupResult {
        // Collect the K closest nodes from entries
        let nodes: Vec<DhtNode> = self
            .entries
            .into_iter()
            .map(|e| e.node)
            .chain(self.discovered_nodes)
            .take(K)
            .collect();

        LookupResult {
            kind: self.kind,
            target: self.target,
            nodes,
            peers: self.discovered_peers,
            tokens: self.tokens,
        }
    }

    /// Get the target node ID.
    pub fn target(&self) -> &NodeId {
        &self.target
    }

    /// Get the lookup kind.
    pub fn kind(&self) -> &LookupKind {
        &self.kind
    }

    /// Get the lookup entries (read-only access for the engine to match
    /// responses to active lookups).
    pub fn entries(&self) -> &VecDeque<LookupEntry> {
        &self.entries
    }
}

// -- DhtLookupTask -----------------------------------------------------------

/// A DHT iterative lookup task (node or peer lookup).
///
/// C++: `DHTNodeLookupTask` / `DHTPeerLookupTask`
pub struct DhtLookupTask {
    state: LookupState,
    started: bool,
}

impl DhtLookupTask {
    /// Create a new lookup task for the given target and kind.
    pub fn new(target: NodeId, kind: LookupKind) -> Self {
        Self {
            state: LookupState::new(target, kind),
            started: false,
        }
    }

    /// Get a mutable reference to the inner lookup state.
    pub fn state_mut(&mut self) -> &mut LookupState {
        &mut self.state
    }

    /// Get a reference to the inner lookup state.
    pub fn state(&self) -> &LookupState {
        &self.state
    }
}

impl DhtTask for DhtLookupTask {
    fn startup(&mut self) {
        self.started = true;
        // startup() on LookupState requires a routing table, which is
        // provided separately via state_mut().startup().
    }

    fn finished(&self) -> bool {
        self.state.is_done()
    }
}
