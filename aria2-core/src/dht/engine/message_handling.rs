//! DHT engine message handling: inbound processing, lookup result handling,
//! and response feeding to active lookups.

use std::net::SocketAddr;

use tracing::{debug, trace};

use super::super::constants::K;
use super::super::message::{AnnouncePeerQueryPayload, DhtMessage};
use super::super::message_decode;
use super::super::node::DhtNode;
use super::super::task::{LookupKind, LookupResult};
use super::DhtEngine;

impl DhtEngine {
    /// Generate a random transaction ID for DHT messages.
    ///
    /// C++: `DHTMessageFactoryImpl::generateTransactionId()` uses random bytes.
    pub(super) fn generate_transaction_id() -> Vec<u8> {
        use super::super::constants::TRANSACTION_ID_LENGTH;
        let mut tid = vec![0u8; TRANSACTION_ID_LENGTH];
        // Use a simple counter + random prefix for uniqueness
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let counter: u32 = (now.as_nanos() as u32) ^ (now.as_secs() as u32);
        tid[..4].copy_from_slice(&counter.to_ne_bytes());
        for b in tid.iter_mut().skip(4) {
            *b = rand::random::<u8>();
        }
        tid
    }

    /// Handle a completed lookup result.
    ///
    /// C++: `DHTPeerLookupTask::onFinish()` + `DHTPeerLookupTask::onReceivedInternal()`
    ///
    /// When a peer lookup finishes:
    /// 1. Add discovered peers to the peer announce storage
    /// 2. Send `announce_peer` messages to the K closest nodes that returned tokens
    ///
    /// When a node lookup finishes:
    /// 1. Add discovered nodes to the routing table
    pub(super) async fn handle_lookup_result(&mut self, result: LookupResult) {
        match result.kind {
            LookupKind::Peer => {
                debug!(
                    info_hash = %result.target,
                    peers = result.peers.len(),
                    tokens = result.tokens.len(),
                    "Peer lookup completed"
                );

                // Feed discovered peers to the peer announce storage
                for peer_addr in &result.peers {
                    self.peer_announce_storage
                        .add_peer_announce(&result.target, *peer_addr);
                }

                // Send announce_peer messages to K closest nodes that provided tokens.
                // C++: DHTPeerLookupTask::onFinish() iterates entries and calls
                // createAnnouncePeerMessage() for each node with a stored token.
                let mut token_count = 0;
                for (node_addr, token) in result.tokens.iter().take(K) {
                    let announce_msg = DhtMessage::AnnouncePeerQuery {
                        transaction_id: Self::generate_transaction_id(),
                        sender_id: self.local_id,
                        sender_addr: *node_addr,
                        payload: AnnouncePeerQueryPayload {
                            info_hash: result.target,
                            port: self.tcp_port,
                            token: token.clone(),
                        },
                    };
                    self.dispatcher.add_message(announce_msg);
                    token_count += 1;
                }
                debug!(
                    info_hash = %result.target,
                    announce_count = token_count,
                    "Queued announce_peer messages"
                );

                // Send the queued messages
                if self.dispatcher.queue_length() > 0 {
                    self.dispatcher.send_messages(&self.transport).await;
                }
            }
            LookupKind::Node => {
                debug!(
                    target = %result.target,
                    nodes = result.nodes.len(),
                    "Node lookup completed"
                );

                // Add discovered nodes to the routing table
                for node in &result.nodes {
                    self.routing_table.add_node(node.clone());
                }
            }
        }
    }

    /// Handle an inbound UDP message.
    pub(super) async fn handle_inbound_message(&mut self, data: &[u8], sender: SocketAddr) {
        // Step 1: Process through receiver
        let actions = self.receiver.receive_message(
            data,
            sender,
            &mut self.dispatcher,
            &mut self.routing_table,
            &mut self.peer_announce_storage,
            &mut self.token_tracker,
        );

        // Step 2: Execute actions (send responses, etc.)
        for action in actions {
            match action {
                super::super::receiver::ReceiveAction::Respond(reply) => {
                    self.dispatcher.add_message(reply);
                }
                super::super::receiver::ReceiveAction::ResponseReceived {
                    method,
                    sender_addr,
                    target_node_id: _,
                    elapsed,
                } => {
                    trace!(
                        method = %method,
                        addr = %sender_addr,
                        elapsed_ms = elapsed.as_millis(),
                        "DHT response processed"
                    );

                    // Feed the response to active lookups.
                    // C++: DHTPeerLookupTask::onReceivedInternal() and
                    // DHTNodeLookupTask handle responses via callbacks.
                    self.feed_response_to_lookups(method.clone(), sender_addr, data);

                    // Feed the response to active ping tasks.
                    // C++: DHTPingTask::onReceived() marks the node as
                    // successfully pinged. For bootstrap pings, this is when
                    // the real node ID becomes known (via the response).
                    self.handle_ping_response(sender_addr, elapsed);

                    // Feed the response to active replace-node tasks.
                    // C++: DHTReplaceNodeTask::onReceived() marks the
                    // questionable node as alive (no replacement needed).
                    self.handle_replace_node_response(sender_addr);
                }
                super::super::receiver::ReceiveAction::NoAction => {}
            }
        }

        // Step 3: Send any queued messages
        self.dispatcher.send_messages(&self.transport).await;

        // Step 4: Execute tasks (may queue more messages)
        self.execute_tasks().await;
    }

    /// Feed an inbound DHT response to active lookups that are waiting for it.
    ///
    /// When a response arrives from a node that an active lookup queried,
    /// the lookup state is updated with the response data (nodes, peers, tokens).
    ///
    /// C++: `DHTAbstractNodeLookupTask` uses a callback mechanism where each
    /// response triggers `onReceived()` -> `onReceivedInternal()`. In Rust,
    /// we match the response to the lookup by the sender address and method.
    pub(super) fn feed_response_to_lookups(
        &mut self,
        method: String,
        sender_addr: SocketAddr,
        data: &[u8],
    ) {
        // Decode the response to extract nodes, peers, and tokens
        let (nodes, peers, token) = match method.as_str() {
            "find_node" => {
                match message_decode::decode_response_with_method(data, sender_addr, "find_node") {
                    Ok(DhtMessage::FindNodeResponse { payload, .. }) => {
                        let nodes: Vec<DhtNode> = payload
                            .nodes
                            .iter()
                            .map(|cni| DhtNode::new(cni.node_id, cni.addr))
                            .collect();
                        (nodes, Vec::new(), None)
                    }
                    _ => (Vec::new(), Vec::new(), None),
                }
            }
            "get_peers" => {
                match message_decode::decode_response_with_method(data, sender_addr, "get_peers") {
                    Ok(DhtMessage::GetPeersResponse { payload, .. }) => {
                        let nodes: Vec<DhtNode> = payload
                            .nodes
                            .iter()
                            .map(|cni| DhtNode::new(cni.node_id, cni.addr))
                            .collect();
                        let peers: Vec<SocketAddr> =
                            payload.values.iter().map(|pi| pi.addr).collect();
                        (nodes, peers, Some(payload.token))
                    }
                    _ => (Vec::new(), Vec::new(), None),
                }
            }
            _ => return,
        };

        // Update the matching active lookup(s)
        for lookup in &mut self.active_lookups {
            let matches_method = match lookup.state().kind() {
                LookupKind::Node => method == "find_node",
                LookupKind::Peer => method == "get_peers",
            };

            if !matches_method {
                continue;
            }

            // Check if the sender is one of the nodes we queried
            let is_queried = lookup
                .state()
                .entries()
                .iter()
                .any(|e| e.node.addr() == sender_addr && e.used);

            if is_queried {
                lookup.state_mut().on_response(
                    sender_addr,
                    nodes.clone(),
                    peers.clone(),
                    token.clone(),
                    &self.local_id,
                );
                trace!(
                    method = %method,
                    addr = %sender_addr,
                    nodes = nodes.len(),
                    peers = peers.len(),
                    "Fed response to active lookup"
                );
            }
        }
    }
}
