//! DHT engine bootstrap, periodic tasks, and persistence.
//!
//! Contains the `DhtEngine` impl methods for:
//! - Bootstrap from entry points
//! - Periodic maintenance (bucket refresh, peer announce, auto-save)
//! - Routing table persistence (load/save)

use std::path::Path;

use tracing::{debug, info, trace, warn};

use super::super::node::DhtNode;
use super::super::routing_table_ser;
use super::super::task::{DhtBucketRefreshTask, DhtPingTask, DhtReplaceNodeTask};
use super::super::transport::AddressFamily;
use super::DhtEngine;

impl DhtEngine {
    /// Bootstrap from configured entry points.
    ///
    /// C++: `DHTEntryPointNameResolveCommand::execute()` resolves the entry
    /// point hostnames, then calls `addPingTask()` for each resolved address.
    /// The ping task verifies the node is alive and updates its real node ID
    /// from the ping response. Only after successful pings does the bootstrap
    /// proceed to `createNodeLookupTask(localNode_->getID())` and
    /// `createBucketRefreshTask()`.
    ///
    /// In the Rust implementation, we create `DhtPingTask` entries with high
    /// retry counts (matching C++'s 10 retries) and send actual ping messages
    /// via the dispatcher. When the response arrives, `handle_inbound_message()`
    /// will update the node in the routing table with the real ID and RTT.
    pub(super) async fn bootstrap(&mut self) {
        info!("Bootstrapping DHT from entry points");

        let entry_points = self.config.entry_points.clone();

        let mut resolved_entries = 0usize;
        for entry in &entry_points {
            // Resolve the entry point hostname. Restrict results to the
            // configured address family, matching C++ NameResolver's family
            // filter instead of accidentally probing both families.
            match tokio::net::lookup_host(format!("{}:{}", entry.host, entry.port)).await {
                Ok(addrs) => {
                    let mut resolved = false;
                    for addr in addrs {
                        let family_matches = match self.config.family {
                            AddressFamily::Ipv4 => addr.is_ipv4(),
                            AddressFamily::Ipv6 => addr.is_ipv6(),
                        };
                        if !family_matches {
                            continue;
                        }
                        resolved = true;
                        info!(addr = %addr, "Pinging DHT entry point before adding");

                        // C++: DHTEntryPointNameResolveCommand::addPingTask()
                        // creates a DHTPingTask with 10 retries. The entry node
                        // starts with a random ID; the real ID comes from the
                        // ping response.
                        let node = DhtNode::with_random_id(addr);
                        let ping_task = DhtPingTask::new(node, 10);
                        self.active_pings.push(ping_task);

                        // Send the ping message immediately
                        self.send_ping(addr);
                    }
                    if resolved {
                        resolved_entries += 1;
                    } else {
                        warn!(
                            host = %entry.host,
                            port = entry.port,
                            family = ?self.config.family,
                            "DHT entry point returned no address in configured family"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        host = %entry.host,
                        port = entry.port,
                        error = %e,
                        "Failed to resolve DHT entry point"
                    );
                }
            }
        }

        if resolved_entries == 0 {
            warn!("No DHT entry points resolved in the configured address family");
            self.bootstrapped = true;
            return;
        }

        // Also trigger a bucket refresh to populate the routing table.
        // C++: DHTBucketRefreshCommand creates bucket refresh tasks which
        // schedule node lookups for stale buckets.
        let mut bucket_refresh = DhtBucketRefreshTask::new(true);
        bucket_refresh.compute_targets(&self.routing_table);
        let targets = bucket_refresh.take_targets();
        for target in targets {
            self.lookup_nodes(target);
        }

        self.bootstrapped = true;
    }

    /// Send a DHT ping message to the given address.
    ///
    /// C++: `DHTPingTask::sendMessage()` dispatches a ping via
    /// `DHTMessageDispatcher::addMessageToQueue()`.
    pub(super) fn send_ping(&mut self, target_addr: std::net::SocketAddr) {
        let transaction_id = Self::generate_transaction_id();
        let msg = super::super::message::DhtMessage::PingQuery {
            transaction_id,
            sender_id: self.local_id,
            sender_addr: target_addr,
            payload: super::super::message::PingQueryPayload,
        };
        self.dispatcher.add_message(msg);
    }

    /// Check if any buckets need refreshing and start node lookups for them.
    ///
    /// C++: `DHTBucketRefreshCommand::execute()` calls
    /// `taskFactory_->createBucketRefreshTask()` which creates a
    /// `DHTBucketRefreshTask`. That task's `startup()` iterates all buckets,
    /// checks `b->needsRefresh()`, calls `b->notifyUpdate()` to reset the
    /// bucket's timer, generates a random target ID within the bucket's range
    /// via `b->getRandomNodeID(targetID)`, then creates a `DHTNodeLookupTask`
    /// for each stale bucket.
    ///
    /// In Rust, we directly create node lookups for stale buckets.
    pub(super) fn bucket_refresh_check(&mut self) {
        // Collect refresh targets from stale buckets
        let mut targets = Vec::new();
        for bucket in self.routing_table.get_buckets() {
            if bucket.needs_refresh() {
                let target = bucket.random_id_in_range();
                debug!(
                    prefix_len = bucket.common_prefix_len(),
                    nodes = bucket.count(),
                    "Bucket needs refresh, scheduling node lookup"
                );
                targets.push(target);
            }
        }

        // Create node lookups for each stale bucket
        for target in targets {
            self.lookup_nodes(target);
        }

        // Check for buckets with questionable nodes and cached replacements.
        // C++: DHTReplaceNodeTask is triggered when a good node is cached in
        // a full bucket. We also proactively check on bucket refresh.
        self.schedule_replace_node_tasks();
    }

    /// Schedule replace-node tasks for buckets with questionable nodes
    /// and cached replacement candidates.
    ///
    /// C++: When `DHTBucket::cacheNode()` is called, the replace-node task
    /// is created to ping the LRU questionable node. If it doesn't respond,
    /// it's replaced with the cached candidate.
    pub(super) fn schedule_replace_node_tasks(&mut self) {
        let buckets = self.routing_table.get_buckets();
        let mut tasks_to_create = Vec::new();

        for bucket in &buckets {
            // Only consider buckets with cached candidates
            if bucket.cached_nodes().is_empty() {
                continue;
            }

            // Find the LRU questionable node
            if let Some(target_node) = bucket.lru_questionable_node() {
                // Get the first cached replacement candidate
                if let Some(replacement) = bucket.cached_nodes().front() {
                    tasks_to_create.push((
                        bucket.common_prefix_len(),
                        target_node.clone(),
                        replacement.as_ref().clone(),
                    ));
                }
            }
        }

        // Create replace-node tasks
        for (prefix_len, target, replacement) in tasks_to_create {
            debug!(
                prefix_len = prefix_len,
                target = %target.addr(),
                replacement = %replacement.addr(),
                "Scheduling replace-node task for questionable node"
            );
            let task = DhtReplaceNodeTask::new(prefix_len, target.clone(), replacement);
            self.active_replace_tasks.push(task);

            // Send a ping to the questionable node
            self.send_ping(target.addr());
        }
    }

    /// Check if any local info hashes need re-announcing.
    pub(super) async fn peer_announce_check(&mut self) {
        // Purge stale peer entries
        self.peer_announce_storage.handle_timeout();

        // Collect info hashes first to avoid immutable borrow conflict
        // when calling self.lookup_peers() (mutable) inside the loop.
        let info_hashes: Vec<super::super::node_id::NodeId> = self
            .peer_announce_storage
            .local_info_hashes()
            .iter()
            .copied()
            .collect();
        for info_hash in info_hashes {
            debug!(info_hash = %info_hash, "Re-announcing local info hash via DHT");
            self.lookup_peers(info_hash);
        }
    }

    /// Save the routing table to disk.
    pub(super) fn auto_save(&self) {
        let path = &self.config.dht_file_path;
        match routing_table_ser::serialize_to_file(&self.routing_table, path) {
            Ok(()) => {
                trace!(path = %path.display(), "Saved DHT routing table");
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to save DHT routing table");
            }
        }
    }

    /// Load or create a local node ID.
    pub(super) fn load_or_create_local_id(dht_file: &Path) -> super::super::node_id::NodeId {
        // Try to load from the routing table file
        match routing_table_ser::deserialize_from_file(dht_file) {
            Ok(result) => {
                debug!(id = %result.local_node_id, "Loaded local node ID from dht.dat");
                result.local_node_id
            }
            Err(e) => {
                debug!(error = %e, "No existing DHT data file, generating new node ID");
                let id = super::super::node_id::NodeId::random();
                debug!(id = %id, "Generated new DHT node ID");
                id
            }
        }
    }

    /// Load the routing table from disk.
    pub(super) fn load_routing_table(
        dht_file: &Path,
        routing_table: &mut super::super::routing_table::RoutingTable,
        _family: AddressFamily,
    ) {
        match routing_table_ser::deserialize_from_file(dht_file) {
            Ok(result) => {
                for node in result.nodes {
                    routing_table.add_node(node);
                }
                debug!("Loaded DHT nodes from routing table file");
            }
            Err(e) => {
                debug!(error = %e, "No DHT routing table file found, starting fresh");
            }
        }
    }
}
