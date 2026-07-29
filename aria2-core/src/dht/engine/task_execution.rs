//! DHT engine task execution: task queue processing, active lookup driving,
//! ping and replace-node task management.

use std::net::SocketAddr;

use tracing::{debug, trace};

use super::super::message::DhtMessage;
use super::super::task::{DhtTask, LookupKind};
use super::DhtEngine;

impl DhtEngine {
    /// Execute pending tasks from the task queue.
    ///
    /// After executing, checks for finished lookup tasks and sends their
    /// results through the lookup result channel. Also drives active
    /// lookup tasks by sending the next batch of queries.
    pub(super) async fn execute_tasks(&mut self) {
        // Execute tasks from the task queue
        self.task_queue.execute();

        // Execute tasks through the task executor
        self.task_executor.update();

        // Drive active lookup tasks: send the next batch of queries
        self.drive_active_lookups().await;

        // Send any messages queued by tasks
        if self.dispatcher.queue_length() > 0 {
            self.dispatcher.send_messages(&self.transport).await;
        }

        // Handle timeouts — updates node state (RTT, condition counter)
        let timed_out = self
            .receiver
            .handle_timeouts(&mut self.dispatcher, &mut self.routing_table);
        if !timed_out.is_empty() {
            // Process timeout entries for active ping and replace-node tasks
            for entry in &timed_out {
                self.handle_ping_timeout(entry.target_addr);
                self.handle_replace_node_timeout(entry.target_addr);
            }
            trace!(count = timed_out.len(), "DHT queries timed out");
        }

        // Clean up completed ping tasks
        let bootstrap_done = self
            .active_pings
            .iter()
            .all(|p| p.finished())
            && !self.active_pings.is_empty();
        if bootstrap_done {
            // All bootstrap pings completed (success or failure).
            // C++: After all entry point pings succeed, the bootstrap
            // triggers createNodeLookupTask(localNode_->getID()).
            let all_success = self.active_pings.iter().all(|p| p.is_success());
            if all_success {
                debug!("All bootstrap entry point pings succeeded, starting initial node lookup");
                self.lookup_nodes(self.local_id);
            } else {
                debug!("Some bootstrap entry point pings failed, proceeding with partial routing table");
                // Still try a node lookup even if some pings failed
                self.lookup_nodes(self.local_id);
            }
            self.active_pings.clear();
        }

        // Clean up completed replace-node tasks
        let mut replacements_to_apply = Vec::new();
        self.active_replace_tasks.retain(|task| {
            if task.finished() && !task.is_target_alive() {
                // Target node is unresponsive — replace it with the candidate
                replacements_to_apply.push((
                    task.bucket_prefix_len(),
                    task.target_node().id().clone(),
                    task.replacement_node().clone(),
                ));
                false // remove from active list
            } else {
                !task.finished() // keep if not finished
            }
        });

        // Apply replacements: drop the bad node, add the replacement
        for (_prefix_len, target_id, replacement) in replacements_to_apply {
            debug!(
                target_id = %target_id,
                replacement_id = %replacement.id(),
                "Replacing unresponsive DHT node with cached candidate"
            );
            self.routing_table.drop_node(&target_id);
            self.routing_table.add_good_node(replacement);
        }
    }

    /// Handle a successful ping response from a node.
    ///
    /// C++: `DHTPingReplyMessage::receivedAction()` calls
    /// `node->markGood()` and `node->updateLastContact()`. For bootstrap
    /// pings, this is where the real node ID (from the response) replaces
    /// the random placeholder ID.
    pub(super) fn handle_ping_response(
        &mut self,
        sender_addr: SocketAddr,
        elapsed: std::time::Duration,
    ) {
        for ping in &mut self.active_pings {
            if ping.remote_node().addr() == sender_addr && !ping.finished() {
                ping.on_response();
                debug!(
                    addr = %sender_addr,
                    elapsed_ms = elapsed.as_millis(),
                    "Bootstrap ping succeeded"
                );
            }
        }
    }

    /// Handle a ping timeout for active ping tasks.
    ///
    /// C++: `DHTPingTask::onTimeout()` retries up to max_retry times.
    pub(super) fn handle_ping_timeout(&mut self, timed_out_addr: SocketAddr) {
        // First pass: update ping task state and collect retry addresses
        let mut retry_addrs = Vec::new();
        for ping in &mut self.active_pings {
            if ping.remote_node().addr() == timed_out_addr && !ping.finished() {
                let should_retry = ping.on_timeout();
                if should_retry {
                    retry_addrs.push(timed_out_addr);
                }
            }
        }
        // Second pass: send retries (avoids borrow conflict)
        for addr in retry_addrs {
            self.send_ping(addr);
            trace!(addr = %addr, "Retrying bootstrap ping");
        }
    }

    /// Handle a successful response for an active replace-node task.
    ///
    /// C++: `DHTReplaceNodeTask::onReceived()` marks the target as alive
    /// and finishes the task without replacing.
    pub(super) fn handle_replace_node_response(&mut self, sender_addr: SocketAddr) {
        for task in &mut self.active_replace_tasks {
            if task.target_node().addr() == sender_addr && !task.finished() {
                task.on_response();
                debug!(
                    addr = %sender_addr,
                    "Replace-node task: questionable node is alive, keeping it"
                );
            }
        }
    }

    /// Handle a timeout for an active replace-node task.
    ///
    /// C++: `DHTReplaceNodeTask::onTimeout()` increments retry count.
    /// After MAX_RETRY timeouts, the target is replaced with the candidate.
    pub(super) fn handle_replace_node_timeout(&mut self, timed_out_addr: SocketAddr) {
        // First pass: update task state and collect retry addresses
        let mut retry_addrs = Vec::new();
        for task in &mut self.active_replace_tasks {
            if task.target_node().addr() == timed_out_addr && !task.finished() {
                task.on_timeout();
                if !task.finished() {
                    retry_addrs.push(timed_out_addr);
                }
            }
        }
        // Second pass: send retries (avoids borrow conflict)
        for addr in retry_addrs {
            self.send_ping(addr);
            trace!(addr = %addr, "Retrying replace-node ping");
        }
    }

    /// Drive active lookup tasks by sending the next batch of queries.
    ///
    /// For each active lookup, this method:
    /// 1. Gets the next batch of nodes to query from `LookupState::next_query_batch()`
    /// 2. Constructs the appropriate DHT query message (find_node or get_peers)
    /// 3. Queues the message via the dispatcher
    /// 4. Marks the queried entries as "used" in the lookup state
    ///
    /// C++: `DHTAbstractNodeLookupTask::sendMessage()` dispatches queries
    /// via `DHTMessageDispatcher::addMessageToQueue()`.
    pub(super) async fn drive_active_lookups(&mut self) {
        // Collect completed lookups by removing them from active_lookups.
        // We cannot use drain_filter directly, so we swap the vec and filter.
        let mut remaining = Vec::new();
        let mut completed_results = Vec::new();

        for lookup in std::mem::take(&mut self.active_lookups) {
            if lookup.is_done() {
                completed_results.push(lookup.into_result());
            } else {
                remaining.push(lookup);
            }
        }
        self.active_lookups = remaining;

        // Handle completed lookup results
        for result in completed_results {
            self.handle_lookup_result(result).await;
        }

        // Drive remaining active lookups: send the next batch of queries
        for lookup in &mut self.active_lookups {
            let batch = lookup.state().next_query_batch();
            if batch.is_empty() {
                continue;
            }

            let indices: Vec<usize> = batch.iter().map(|(i, _)| *i).collect();
            let method = match lookup.state().kind() {
                LookupKind::Node => "find_node",
                LookupKind::Peer => "get_peers",
            };

            for (_, node) in &batch {
                let transaction_id = Self::generate_transaction_id();
                let target_addr = node.addr();

                let msg = match lookup.state().kind() {
                    LookupKind::Node => {
                        let target = *lookup.state().target();
                        DhtMessage::FindNodeQuery {
                            transaction_id,
                            sender_id: self.local_id,
                            sender_addr: target_addr,
                            payload: super::super::message::FindNodeQueryPayload { target },
                        }
                    }
                    LookupKind::Peer => {
                        let info_hash = *lookup.state().target();
                        DhtMessage::GetPeersQuery {
                            transaction_id,
                            sender_id: self.local_id,
                            sender_addr: target_addr,
                            payload: super::super::message::GetPeersQueryPayload { info_hash },
                        }
                    }
                };

                trace!(
                    method = method,
                    addr = %target_addr,
                    tid = ?msg.transaction_id(),
                    "Dispatching lookup query"
                );

                self.dispatcher.add_message(msg);
            }

            // Mark the queried entries as "used" in the lookup state
            lookup.state_mut().mark_sent(&indices);
        }

        // Send any queued messages
        if self.dispatcher.queue_length() > 0 {
            self.dispatcher.send_messages(&self.transport).await;
        }
    }
}
