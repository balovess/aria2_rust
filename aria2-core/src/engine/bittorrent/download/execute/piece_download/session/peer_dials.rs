use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::interaction::BtPeerConnectionOptions;
use crate::request::request_group::BtPeerSource;
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::bittorrent::peer::connection::PeerAddr;

type PeerEndpoint = (String, u16);
const MAX_QUEUED_PEER_DIALS: usize = crate::engine::bittorrent::peer::storage::MAX_PEER_LIST_SIZE;

/// Immutable connection parameters shared by background handshake attempts.
#[derive(Clone)]
pub(in crate::engine::bittorrent::download::execute) struct PeerDialConfig {
    pub(in crate::engine::bittorrent::download::execute) connection_options:
        BtPeerConnectionOptions,
    pub(in crate::engine::bittorrent::download::execute) info_hash: [u8; 20],
    pub(in crate::engine::bittorrent::download::execute) num_pieces: u32,
    pub(in crate::engine::bittorrent::download::execute) piece_length: u32,
    pub(in crate::engine::bittorrent::download::execute) total_size: u64,
    pub(in crate::engine::bittorrent::download::execute) max_concurrent_dials: usize,
    pub(in crate::engine::bittorrent::download::execute) utp_socket:
        Option<Arc<tokio::sync::Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
    pub(in crate::engine::bittorrent::download::execute) outbound_network_policy:
        Arc<crate::network::OutboundNetworkPolicy>,
}

impl PeerDialConfig {
    pub(in crate::engine::bittorrent::download::execute) fn new(
        command: &BtDownloadCommand,
        info_hash: [u8; 20],
        hybrid_info_hash_v2: Option<[u8; 32]>,
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
    ) -> Self {
        let group = command.group.recover();
        let mut connection_options =
            BtPeerConnectionOptions::from_download_options(group.options(), command.local_peer_id);
        connection_options.dht_enabled =
            (group.options().enable_dht || group.options().enable_dht6) && !command.is_private;
        connection_options.listen_port = (command.listen_port != 0).then_some(command.listen_port);
        connection_options.hybrid_info_hash_v2 = hybrid_info_hash_v2;
        drop(group);

        Self {
            connection_options,
            info_hash,
            num_pieces,
            piece_length,
            total_size,
            max_concurrent_dials: command.peer_coordinator.max_concurrent_dials(),
            utp_socket: command.utp_socket.clone(),
            outbound_network_policy: Arc::clone(&command.outbound_network_policy),
        }
    }
}

/// Owns queued, pre-actor handshakes for one piece-session lifetime.
///
/// Established connections are returned to the session coordinator and become
/// long-lived Swarm actors there. Dropping this queue aborts its active batch.
pub(super) struct PeerDialQueue {
    candidates: VecDeque<(PeerAddr, BtPeerSource)>,
    pending_endpoints: HashSet<PeerEndpoint>,
    active_batch_endpoints: Vec<PeerEndpoint>,
    tasks: tokio::task::JoinSet<Vec<BtPeerConn>>,
}

impl Default for PeerDialQueue {
    fn default() -> Self {
        Self {
            candidates: VecDeque::new(),
            pending_endpoints: HashSet::new(),
            active_batch_endpoints: Vec::new(),
            tasks: tokio::task::JoinSet::new(),
        }
    }
}

impl PeerDialQueue {
    pub(super) fn enqueue(&mut self, peers: Vec<PeerAddr>, source: BtPeerSource) -> usize {
        let mut added = 0;
        for peer in peers {
            let key = (peer.ip.clone(), peer.port);
            if self.pending_endpoints.contains(&key) {
                continue;
            }
            if self.pending_endpoints.len() >= MAX_QUEUED_PEER_DIALS {
                break;
            }
            self.pending_endpoints.insert(key);
            self.candidates.push_back((peer, source));
            added += 1;
        }
        added
    }

    pub(super) fn start_next(&mut self, max_connections: usize, config: &PeerDialConfig) -> bool {
        if max_connections == 0 || !self.tasks.is_empty() || self.candidates.is_empty() {
            return false;
        }

        let source = self.candidates.front().expect("queue is not empty").1;
        let batch_limit = config.max_concurrent_dials.max(1);
        let mut peers = Vec::with_capacity(batch_limit);
        while peers.len() < batch_limit
            && self
                .candidates
                .front()
                .is_some_and(|(_, candidate_source)| *candidate_source == source)
        {
            let (peer, _) = self.candidates.pop_front().expect("front was checked");
            self.active_batch_endpoints
                .push((peer.ip.clone(), peer.port));
            peers.push(peer);
        }

        let config = config.clone();
        self.tasks.spawn(async move {
            super::super::super::pex::connect_discovered_peers(
                peers,
                source,
                max_connections,
                config,
            )
            .await
        });
        true
    }

    pub(super) fn has_active_batch(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub(super) fn try_join_next(
        &mut self,
    ) -> Option<std::result::Result<Vec<BtPeerConn>, tokio::task::JoinError>> {
        let result = self.tasks.try_join_next();
        if result.is_some() {
            self.finish_active_batch();
        }
        result
    }

    pub(super) async fn join_next(
        &mut self,
    ) -> Option<std::result::Result<Vec<BtPeerConn>, tokio::task::JoinError>> {
        let result = self.tasks.join_next().await;
        if result.is_some() {
            self.finish_active_batch();
        }
        result
    }

    fn finish_active_batch(&mut self) {
        for endpoint in self.active_batch_endpoints.drain(..) {
            self.pending_endpoints.remove(&endpoint);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_QUEUED_PEER_DIALS, PeerDialQueue};
    use crate::request::request_group::BtPeerSource;
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;

    #[test]
    fn queued_peer_dials_are_bounded_by_peer_storage_capacity() {
        let mut queue = PeerDialQueue::default();
        let peers = (1..=MAX_QUEUED_PEER_DIALS + 1)
            .map(|port| PeerAddr::new("127.0.0.1", port as u16))
            .collect();

        assert_eq!(
            queue.enqueue(peers, BtPeerSource::Pex),
            MAX_QUEUED_PEER_DIALS
        );
        assert_eq!(queue.candidates.len(), MAX_QUEUED_PEER_DIALS);
        assert_eq!(queue.pending_endpoints.len(), MAX_QUEUED_PEER_DIALS);
    }
}
