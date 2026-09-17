use futures::stream::{FuturesUnordered, StreamExt};
use std::net::SocketAddr;
use tracing::debug;

use super::super::lookup::{
    iterative_get_item, iterative_get_item_for_publish, iterative_sample_infohashes,
};
use super::super::modern::{
    MutableValue, SampleInfoHashesResponse, StoredItem, put_immutable_query, put_query,
};
use super::super::node::DhtNode;
use super::super::tracker::QueryType;
use super::{DhtEngine, DhtEngineState, FindPeersResult};

impl DhtEngine {
    /// Look up peers for the given info hash via the DHT network.
    ///
    /// Performs an iterative `get_peers` lookup with alpha-parallelism,
    /// returning discovered peer addresses.
    pub async fn find_peers(&self, info_hash: &[u8; 20]) -> std::io::Result<FindPeersResult> {
        let state = self.state().await;
        if state != DhtEngineState::Running && state != DhtEngineState::Bootstrapping {
            return Ok(FindPeersResult {
                peers: vec![],
                nodes_contacted: 0,
            });
        }

        debug!(info_hash = %hex::encode(info_hash), "Starting DHT get_peers lookup");

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let accepted = self
            .task_queue
            .add_immediate_task(self.context.task_factory.create_peer_lookup_task(
                *info_hash,
                0,
                Some(result_tx),
            ))
            .await;
        if !accepted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT peer lookup task was cancelled",
            ));
        }
        let result = result_rx.await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT peer lookup task was cancelled",
            )
        })?;

        Ok(FindPeersResult {
            peers: result.peers,
            nodes_contacted: result.nodes_contacted,
        })
    }

    /// Announce that we are serving the torrent identified by `info_hash` on `port`.
    ///
    /// Performs a `get_peers` lookup first to obtain tokens, then sends
    /// `announce_peer` queries to the closest K nodes that provided tokens.
    pub async fn announce_peer(&self, info_hash: &[u8; 20], port: u16) -> std::io::Result<()> {
        let state = self.state().await;
        if state != DhtEngineState::Running && state != DhtEngineState::Bootstrapping {
            return Ok(());
        }

        debug!(
            info_hash = %hex::encode(info_hash),
            port,
            "Starting DHT announce_peer"
        );

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let accepted = self
            .task_queue
            .add_immediate_task(self.context.task_factory.create_peer_lookup_task(
                *info_hash,
                port,
                Some(result_tx),
            ))
            .await;
        if !accepted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT announce task was cancelled",
            ));
        }
        result_rx.await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT announce task was cancelled",
            )
        })?;

        Ok(())
    }

    /// Query one nearby DHT node for a BEP 51 sample of its stored info-hashes.
    ///
    /// The returned response is validated before being exposed to callers.
    pub async fn sample_infohashes(
        &self,
        target: &[u8; 20],
    ) -> std::io::Result<Option<SampleInfoHashesResponse>> {
        if !matches!(
            self.state().await,
            DhtEngineState::Running | DhtEngineState::Bootstrapping
        ) {
            return Ok(None);
        }
        Ok(iterative_sample_infohashes(
            target,
            &self.context.handler_self_id,
            &self.context.routing_table,
            &self.context.socket,
            &self.context.tracker,
            self.context.config.query_timeout,
        )
        .await
        .response)
    }

    /// Fetch a BEP 44 item from the closest known node.
    pub async fn get_item(
        &self,
        target: &[u8; 20],
        seq: Option<i64>,
    ) -> std::io::Result<Option<StoredItem>> {
        Ok(iterative_get_item(
            target,
            seq,
            &self.context.handler_self_id,
            &self.context.routing_table,
            &self.context.socket,
            &self.context.tracker,
            self.context.config.query_timeout,
        )
        .await
        .item
        .filter(StoredItem::verify_target))
    }

    pub async fn put_immutable(
        &self,
        value: &crate::bittorrent::bencode::codec::BencodeValue,
    ) -> std::io::Result<bool> {
        let target = StoredItem::immutable_target(value);
        let lookup = iterative_get_item_for_publish(
            &target,
            None,
            &self.context.handler_self_id,
            &self.context.routing_table,
            &self.context.socket,
            &self.context.tracker,
            self.context.config.query_timeout,
        )
        .await;
        self.publish_immutable_to_tokens(&lookup.token_nodes, value)
            .await
    }

    /// Publish a signed BEP 44 mutable item after obtaining a fresh token.
    pub async fn put_mutable(
        &self,
        item: &MutableValue,
        cas: Option<i64>,
    ) -> std::io::Result<bool> {
        if !item.verify_signature() {
            return Ok(false);
        }
        let target = StoredItem::mutable_target(&item.public_key, item.salt.as_deref());
        let lookup = iterative_get_item_for_publish(
            &target,
            None,
            &self.context.handler_self_id,
            &self.context.routing_table,
            &self.context.socket,
            &self.context.tracker,
            self.context.config.query_timeout,
        )
        .await;
        self.publish_mutable_to_tokens(&lookup.token_nodes, item, cas)
            .await
    }

    async fn publish_immutable_to_tokens(
        &self,
        token_nodes: &[(SocketAddr, [u8; 20], Vec<u8>)],
        value: &crate::bittorrent::bencode::codec::BencodeValue,
    ) -> std::io::Result<bool> {
        let mut sends = FuturesUnordered::new();
        for (addr, node_id, token) in token_nodes.iter().take(8) {
            let (tx, wait) = self.context.tracker.allocate_wait(
                QueryType::Put,
                *addr,
                Some(*node_id),
                None,
                self.context.config.query_timeout,
            );
            let message = put_immutable_query(tx, &self.context.handler_self_id, token, value);
            let encoded = message.encode().map_err(std::io::Error::other)?;
            let socket = self.context.socket.clone();
            let timeout = self.context.config.query_timeout;
            let addr = *addr;
            sends.push(async move {
                socket.send_to(addr, &encoded).await.is_ok()
                    && wait
                        .wait(timeout)
                        .await
                        .is_some_and(|response| response.message.is_response())
            });
        }
        let mut accepted = false;
        while let Some(result) = sends.next().await {
            accepted |= result;
        }
        Ok(accepted)
    }

    async fn publish_mutable_to_tokens(
        &self,
        token_nodes: &[(SocketAddr, [u8; 20], Vec<u8>)],
        item: &MutableValue,
        cas: Option<i64>,
    ) -> std::io::Result<bool> {
        let mut sends = FuturesUnordered::new();
        for (addr, node_id, token) in token_nodes.iter().take(8) {
            let (tx, wait) = self.context.tracker.allocate_wait(
                QueryType::Put,
                *addr,
                Some(*node_id),
                None,
                self.context.config.query_timeout,
            );
            let message = put_query(tx, &self.context.handler_self_id, token, item, cas);
            let encoded = message.encode().map_err(std::io::Error::other)?;
            let socket = self.context.socket.clone();
            let timeout = self.context.config.query_timeout;
            let addr = *addr;
            sends.push(async move {
                socket.send_to(addr, &encoded).await.is_ok()
                    && wait
                        .wait(timeout)
                        .await
                        .is_some_and(|response| response.message.is_response())
            });
        }
        let mut accepted = false;
        while let Some(result) = sends.next().await {
            accepted |= result;
        }
        Ok(accepted)
    }

    /// Add a bootstrap node to the routing table.
    ///
    /// Sends a `ping` to `addr` and inserts it into the appropriate k-bucket
    /// once a response is received.
    pub async fn add_node(&self, addr: SocketAddr) {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let accepted = self
            .task_queue
            .add_immediate_task(self.context.task_factory.create_ping_task_with_result(
                DhtNode::new([0u8; 20], addr),
                0,
                result_tx,
            ))
            .await;
        if !accepted {
            return;
        }

        if let Ok(Some(node)) = result_rx.await {
            debug!(
                addr = %addr,
                id = %hex::encode(node.id),
                "Added DHT node via add_node"
            );
        }
    }
}
