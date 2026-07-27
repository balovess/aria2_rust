//! DHT message receiver for inbound messages.
//!
//! Receives raw UDP datagrams, decodes them into DHT message types,
//! and processes them by updating the routing table and dispatching
//! automatic responses (e.g., ping replies, find_node replies).
//!
//! C++ reference: `DHTMessageReceiver.h/cc`

use std::net::SocketAddr;

use tracing::{debug, trace, warn};

use super::dispatcher::DhtDispatcher;
use super::message::DhtMessage;
use super::message_decode;
use super::node::DhtNode;
use super::node_id::NodeId;
use super::peer_announce::DhtPeerAnnounceStorage;
use super::routing_table::RoutingTable;
use super::token_tracker::TokenTracker;

// ── ReceiveAction ─────────────────────────────────────────────────────────

/// Result of processing a received DHT message.
#[derive(Debug)]
pub enum ReceiveAction {
    /// The message was an inbound query; a response should be sent.
    Respond(DhtMessage),
    /// The message was an inbound response; the routing table was updated.
    ResponseReceived {
        method: String,
        sender_addr: SocketAddr,
    },
    /// The message was an error or unknown; no action needed.
    NoAction,
}

// ── DhtReceiver ───────────────────────────────────────────────────────────

/// Receives and processes inbound DHT messages.
///
/// The receiver decodes raw UDP datagrams into [`DhtMessage`] values and
/// handles them according to their type:
///
/// - **Queries**: Update the routing table with the sender's node info,
///   then return a [`ReceiveAction::Respond`] with the appropriate reply.
/// - **Responses**: Match the response to a tracked outbound query via
///   the dispatcher's tracker, then update the routing table.
/// - **Errors**: Log and discard.
///
/// C++: `DHTMessageReceiver`
pub struct DhtReceiver {
    /// The local node's ID.
    local_id: NodeId,
}

impl DhtReceiver {
    /// Create a new receiver for the given local node ID.
    pub fn new(local_id: NodeId) -> Self {
        Self { local_id }
    }

    /// Process a received raw DHT datagram.
    ///
    /// Decodes the message, matches responses to tracked queries,
    /// and returns the appropriate action for the caller to take.
    pub fn receive_message(
        &self,
        data: &[u8],
        remote_addr: SocketAddr,
        dispatcher: &mut DhtDispatcher,
        routing_table: &mut RoutingTable,
        peer_announce_storage: &mut DhtPeerAnnounceStorage,
        token_tracker: &mut TokenTracker,
    ) -> Vec<ReceiveAction> {
        let mut actions = Vec::new();

        // Step 1: Is this a response to a tracked query?
        // Peek at the transaction ID first without decoding the full message.
        if let Some(tid) = Self::extract_transaction_id(data) {
            if let Some(tracked) = dispatcher.take_tracked(&tid, remote_addr) {
                // This is a response to one of our queries
                let method = tracked.method.clone();
                trace!(
                    tid = ?tid,
                    addr = %remote_addr,
                    method = %method,
                    "Received DHT response matching tracked query"
                );

                // Decode the response using the known method
                match message_decode::decode_response_with_method(data, remote_addr, &method) {
                    Ok(msg) => {
                        self.handle_response(
                            &msg,
                            remote_addr,
                            routing_table,
                            peer_announce_storage,
                            &mut actions,
                        );
                    }
                    Err(e) => {
                        warn!(
                            addr = %remote_addr,
                            method = %method,
                            error = %e,
                            "Failed to decode DHT response"
                        );
                    }
                }
                return actions;
            }
        }

        // Step 2: Try to decode as a new query or error
        match message_decode::decode(data, remote_addr) {
            Ok(msg) => {
                self.handle_query(
                    &msg,
                    remote_addr,
                    routing_table,
                    peer_announce_storage,
                    token_tracker,
                    &mut actions,
                );
            }
            Err(e) => {
                // Could be a response with an unknown transaction ID,
                // or a malformed message.
                trace!(
                    addr = %remote_addr,
                    error = %e,
                    "Failed to decode DHT message (may be unknown response)"
                );
            }
        }

        actions
    }

    /// Handle a decoded response message.
    fn handle_response(
        &self,
        msg: &DhtMessage,
        _remote_addr: SocketAddr,
        routing_table: &mut RoutingTable,
        peer_announce_storage: &mut DhtPeerAnnounceStorage,
        actions: &mut Vec<ReceiveAction>,
    ) {
        // Add the responding node to the routing table
        if let Some(sender_id) = msg.sender_id() {
            let node = DhtNode::new(*sender_id, *msg.sender_addr());
            routing_table.add_node(node);
        }

        // Handle get_peers responses: store discovered peers
        if let DhtMessage::GetPeersResponse { payload, .. } = msg {
            for peer_info in &payload.values {
                peer_announce_storage.add_peer_announce(
                    // We don't know the info hash from the response alone,
                    // but the task system does. For now, just add the peer.
                    // This will be properly wired via the task callback.
                    &NodeId::ZERO, // placeholder
                    peer_info.addr,
                );
            }
        }

        actions.push(ReceiveAction::ResponseReceived {
            method: msg.method_name().unwrap_or("unknown").to_owned(),
            sender_addr: *msg.sender_addr(),
        });
    }

    /// Handle a decoded query message.
    fn handle_query(
        &self,
        msg: &DhtMessage,
        remote_addr: SocketAddr,
        routing_table: &mut RoutingTable,
        peer_announce_storage: &mut DhtPeerAnnounceStorage,
        token_tracker: &mut TokenTracker,
        actions: &mut Vec<ReceiveAction>,
    ) {
        // Add the querying node to the routing table
        if let Some(sender_id) = msg.sender_id() {
            let node = DhtNode::new(*sender_id, remote_addr);
            routing_table.add_node(node);
        }

        // Generate the appropriate response
        match msg {
            DhtMessage::PingQuery {
                transaction_id,
                sender_id: _,
                ..
            } => {
                debug!(tid = ?transaction_id, addr = %remote_addr, "Received DHT ping query");
                let reply = DhtMessage::PingResponse {
                    transaction_id: transaction_id.clone(),
                    sender_id: self.local_id,
                    sender_addr: remote_addr,
                    payload: super::message::PingResponsePayload,
                };
                actions.push(ReceiveAction::Respond(reply));
            }

            DhtMessage::FindNodeQuery {
                transaction_id,
                sender_id: _,
                payload,
                ..
            } => {
                debug!(
                    tid = ?transaction_id,
                    addr = %remote_addr,
                    target = %payload.target,
                    "Received DHT find_node query"
                );
                let closest = routing_table.get_closest_k_nodes(&payload.target);
                let nodes: Vec<super::message::CompactNodeInfo> = closest
                    .into_iter()
                    .map(|n| super::message::CompactNodeInfo {
                        node_id: *n.id(),
                        addr: n.addr(),
                    })
                    .collect();

                let reply = DhtMessage::FindNodeResponse {
                    transaction_id: transaction_id.clone(),
                    sender_id: self.local_id,
                    sender_addr: remote_addr,
                    payload: super::message::FindNodeResponsePayload { nodes },
                };
                actions.push(ReceiveAction::Respond(reply));
            }

            DhtMessage::GetPeersQuery {
                transaction_id,
                sender_id: _,
                payload,
                ..
            } => {
                debug!(
                    tid = ?transaction_id,
                    addr = %remote_addr,
                    info_hash = %payload.info_hash,
                    "Received DHT get_peers query"
                );

                // Generate a token for this IP and port
                let ip_str = remote_addr.ip().to_string();
                let port = remote_addr.port();
                let token = token_tracker.generate_token(payload.info_hash.as_bytes(), &ip_str, port);

                // Check if we have peers for this info hash
                let peers = peer_announce_storage.get_peers(&payload.info_hash);
                let values: Vec<super::message::CompactPeerInfo> = peers
                    .into_iter()
                    .map(|addr| super::message::CompactPeerInfo { addr })
                    .collect();

                // Also return closest nodes
                let closest = routing_table.get_closest_k_nodes(&payload.info_hash);
                let nodes: Vec<super::message::CompactNodeInfo> = closest
                    .into_iter()
                    .map(|n| super::message::CompactNodeInfo {
                        node_id: *n.id(),
                        addr: n.addr(),
                    })
                    .collect();

                let reply = DhtMessage::GetPeersResponse {
                    transaction_id: transaction_id.clone(),
                    sender_id: self.local_id,
                    sender_addr: remote_addr,
                    payload: super::message::GetPeersResponsePayload {
                        token,
                        nodes,
                        values,
                    },
                };
                actions.push(ReceiveAction::Respond(reply));
            }

            DhtMessage::AnnouncePeerQuery {
                transaction_id,
                sender_id: _,
                payload,
                ..
            } => {
                debug!(
                    tid = ?transaction_id,
                    addr = %remote_addr,
                    info_hash = %payload.info_hash,
                    port = payload.port,
                    "Received DHT announce_peer query"
                );

                // Validate the token
                let ip_str = remote_addr.ip().to_string();
                let port = remote_addr.port();
                if token_tracker.validate_token(&payload.token, payload.info_hash.as_bytes(), &ip_str, port) {
                    // Store the announced peer
                    let announce_addr = if payload.port > 0 {
                        SocketAddr::new(remote_addr.ip(), payload.port)
                    } else {
                        remote_addr
                    };
                    peer_announce_storage.add_peer_announce(&payload.info_hash, announce_addr);

                    let reply = DhtMessage::AnnouncePeerResponse {
                        transaction_id: transaction_id.clone(),
                        sender_id: self.local_id,
                        sender_addr: remote_addr,
                        payload: super::message::AnnouncePeerResponsePayload,
                    };
                    actions.push(ReceiveAction::Respond(reply));
                } else {
                    warn!(
                        addr = %remote_addr,
                        "Invalid token in announce_peer query, ignoring"
                    );
                }
            }

            DhtMessage::Error { code, message, .. } => {
                warn!(
                    addr = %remote_addr,
                    code = code,
                    msg = %message,
                    "Received DHT error message"
                );
                actions.push(ReceiveAction::NoAction);
            }

            // Response messages should have been handled by handle_response
            _ => {
                trace!(addr = %remote_addr, "Unexpected message type in query handler");
            }
        }
    }

    /// Extract the transaction ID from a raw bencoded DHT message.
    ///
    /// Returns `None` if the message doesn't contain a "t" key or if
    /// parsing fails.
    fn extract_transaction_id(data: &[u8]) -> Option<Vec<u8>> {
        // Quick bencode parse: find "1:t" followed by the length and bytes
        // This is a fast path to avoid full decode for response matching.
        // The full decode happens later if we need to process the message.

        // Find "1:t" in the data
        let pattern = b"1:t";
        let start = data.windows(3).position(|w| w == pattern)?;

        // After "1:t", expect a bencode integer length (e.g., "4:") followed by bytes
        let after = &data[start + 3..];

        // Parse the length
        let colon_pos = after.iter().position(|&b| b == b':')?;
        let len_str = std::str::from_utf8(&after[..colon_pos]).ok()?;
        let len: usize = len_str.parse().ok()?;

        // Extract the bytes
        let tid_start = colon_pos + 1;
        let tid_end = tid_start + len;
        if tid_end > after.len() {
            return None;
        }
        Some(after[tid_start..tid_end].to_vec())
    }

    /// Handle message tracker timeouts.
    ///
    /// Called periodically to clean up unanswered queries. Returns the
    /// list of timed-out entries for the caller to handle (e.g., marking
    /// nodes as bad in the routing table).
    pub fn handle_timeouts(
        &self,
        dispatcher: &mut DhtDispatcher,
        _routing_table: &mut RoutingTable,
    ) -> Vec<super::tracker::TimeoutEntry> {
        let timed_out = dispatcher.handle_timeouts();

        for entry in &timed_out {
            debug!(
                addr = %entry.target_addr,
                method = %entry.method,
                "DHT query timed out"
            );

            // Move the timed-out node to the tail of its bucket (soft penalty).
            // If it fails too many times, it will be evicted in favor of
            // replacement candidates.
            // We don't have the node_id here, so we rely on the routing
            // table's own health tracking during bucket refreshes.
        }

        timed_out
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::constants::ID_LENGTH;

    fn test_local_id() -> NodeId {
        NodeId([0xAA; ID_LENGTH])
    }

    fn test_addr() -> SocketAddr {
        "192.168.0.1:6881".parse().unwrap()
    }

    fn test_sender_id() -> NodeId {
        NodeId([0xBB; ID_LENGTH])
    }

    #[test]
    fn extract_tid_from_ping_query() {
        // Minimal bencoded ping: d1:t4:abcd1:y1:q1:q4:pinge
        let data = b"d1:t4:abcd1:y1:q1:q4:pinge";
        let tid = DhtReceiver::extract_transaction_id(data);
        assert_eq!(tid, Some(b"abcd".to_vec()));
    }

    #[test]
    fn extract_tid_from_response() {
        // Minimal bencoded response: d1:t2:xx1:y1:re
        let data = b"d1:t2:xx1:y1:re";
        let tid = DhtReceiver::extract_transaction_id(data);
        assert_eq!(tid, Some(b"xx".to_vec()));
    }

    #[test]
    fn extract_tid_missing_returns_none() {
        let data = b"d1:y1:qe";
        let tid = DhtReceiver::extract_transaction_id(data);
        assert!(tid.is_none());
    }

    #[test]
    fn extract_tid_empty_data() {
        let tid = DhtReceiver::extract_transaction_id(b"");
        assert!(tid.is_none());
    }

    #[test]
    fn new_receiver_has_local_id() {
        let r = DhtReceiver::new(test_local_id());
        assert_eq!(r.local_id, test_local_id());
    }
}
