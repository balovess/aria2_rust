//! The process DHT engines, indexed by their bound IP family.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use aria2_protocol::bittorrent::dht::engine::DhtEngine;

#[derive(Clone, Default)]
pub(crate) struct DhtEngineSet {
    ipv4: Option<Arc<DhtEngine>>,
    ipv6: Option<Arc<DhtEngine>>,
}

impl DhtEngineSet {
    pub(crate) fn attach_global_if_present(
        &mut self,
        registry: Option<&Arc<std::sync::RwLock<crate::engine::bittorrent::registry::BtRegistry>>>,
        gid: u64,
        use_ipv6: bool,
    ) -> bool {
        let Some(registry) = registry else {
            return false;
        };
        let engine = registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .attach_global_dht_engine_for_gid(gid, use_ipv6);
        let Some(engine) = engine else {
            return false;
        };
        self.insert(engine);
        true
    }

    pub(crate) async fn start_or_join(
        &mut self,
        registry: Option<&Arc<std::sync::RwLock<crate::engine::bittorrent::registry::BtRegistry>>>,
        gid: u64,
        config: aria2_protocol::bittorrent::dht::engine::DhtEngineConfig,
    ) -> io::Result<()> {
        let engine = match registry {
            Some(registry) => {
                crate::engine::bittorrent::registry::BtRegistry::get_or_start_global_dht_engine(
                    registry, gid, config,
                )
                .await?
            }
            None => DhtEngine::start(config).await?,
        };
        self.insert(engine);
        Ok(())
    }

    pub(crate) fn insert(&mut self, engine: Arc<DhtEngine>) -> Option<Arc<DhtEngine>> {
        let slot = if engine.local_addr().is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };
        slot.replace(engine)
    }

    pub(crate) fn ipv4(&self) -> Option<Arc<DhtEngine>> {
        self.ipv4.as_ref().map(Arc::clone)
    }

    pub(crate) fn ipv6(&self) -> Option<Arc<DhtEngine>> {
        self.ipv6.as_ref().map(Arc::clone)
    }

    pub(crate) fn for_peer(&self, address: SocketAddr) -> Option<Arc<DhtEngine>> {
        if address.is_ipv6() {
            self.ipv6()
        } else {
            self.ipv4()
        }
    }

    pub(crate) fn remove_if(&mut self, engine: &Arc<DhtEngine>) -> bool {
        let slot = if engine.local_addr().is_ipv6() {
            &mut self.ipv6
        } else {
            &mut self.ipv4
        };
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, engine))
        {
            *slot = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ipv4.is_none() && self.ipv6.is_none()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<DhtEngine>> {
        self.ipv4.iter().chain(self.ipv6.iter())
    }

    pub(crate) fn into_vec(self) -> Vec<Arc<DhtEngine>> {
        self.ipv4.into_iter().chain(self.ipv6).collect()
    }

    pub(crate) async fn find_peers_and_announce(
        &self,
        info_hash: &[u8; 20],
        announce_port: u16,
    ) -> io::Result<aria2_protocol::bittorrent::dht::engine::FindPeersResult> {
        self.lookup_peers(info_hash, Some(announce_port)).await
    }

    pub(crate) async fn find_peers(
        &self,
        info_hash: &[u8; 20],
    ) -> io::Result<aria2_protocol::bittorrent::dht::engine::FindPeersResult> {
        self.lookup_peers(info_hash, None).await
    }

    async fn lookup_peers(
        &self,
        info_hash: &[u8; 20],
        announce_port: Option<u16>,
    ) -> io::Result<aria2_protocol::bittorrent::dht::engine::FindPeersResult> {
        let engines = self.iter().cloned().collect::<Vec<_>>();
        if engines.is_empty() {
            return Ok(aria2_protocol::bittorrent::dht::engine::FindPeersResult {
                peers: Vec::new(),
                nodes_contacted: 0,
            });
        }

        let results = futures::future::join_all(engines.iter().map(|engine| async move {
            match announce_port {
                Some(port) => engine.find_peers_and_announce(info_hash, port).await,
                None => engine.find_peers(info_hash).await,
            }
        }))
        .await;
        let mut peers = Vec::new();
        let mut nodes_contacted = 0;
        let mut successful = false;
        let mut first_error = None;
        for result in results {
            match result {
                Ok(result) => {
                    successful = true;
                    nodes_contacted += result.nodes_contacted;
                    for peer in result.peers {
                        if !peers.contains(&peer) {
                            peers.push(peer);
                        }
                    }
                }
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }

        if successful {
            Ok(aria2_protocol::bittorrent::dht::engine::FindPeersResult {
                peers,
                nodes_contacted,
            })
        } else {
            Err(first_error.expect("at least one engine failed"))
        }
    }

    pub(crate) async fn announce_peer(
        &self,
        info_hash: &[u8; 20],
        port: u16,
    ) -> Vec<(SocketAddr, io::Result<()>)> {
        let engines = self.iter().cloned().collect::<Vec<_>>();
        futures::future::join_all(engines.iter().map(|engine| async move {
            (
                engine.local_addr(),
                engine.announce_peer(info_hash, port).await,
            )
        }))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::bittorrent::dht::engine::{DhtEngine, DhtEngineConfig};
    use aria2_protocol::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn periodic_lookup_aggregator_announces_with_lookup_token() {
        const INFO_HASH: [u8; 20] = [0x35; 20];
        const ANNOUNCE_PORT: u16 = 51413;
        let node_id = [0xD3; 20];
        let peer: SocketAddr = "127.0.0.1:6882".parse().expect("fixture peer address");
        let responder = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("local DHT responder should bind");
        let responder_addr = responder.local_addr().expect("responder address");
        let responder_task = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut get_peers_count = 0;
            loop {
                let (len, from) =
                    tokio::time::timeout(Duration::from_secs(2), responder.recv_from(&mut buf))
                        .await
                        .expect("aggregated lookup should finish its UDP exchange")
                        .expect("responder should receive DHT query");
                let query = DhtMessage::decode(&buf[..len]).expect("valid DHT query");
                let (response, announced) = match query.q.as_ref().map(|method| method.0.as_str()) {
                    Some("ping") => (DhtMessageBuilder::ping_response(&query.t, &node_id), false),
                    Some("find_node") => (
                        DhtMessageBuilder::find_node_response(&query.t, &node_id, &[]),
                        false,
                    ),
                    Some("get_peers") => {
                        get_peers_count += 1;
                        let args = query.a.as_ref().expect("get_peers arguments");
                        assert_eq!(
                            args.dict_get(b"info_hash")
                                .and_then(|value| value.as_bytes()),
                            Some(&INFO_HASH[..])
                        );
                        (
                            DhtMessageBuilder::get_peers_response_with_peers(
                                &query.t,
                                &node_id,
                                b"lookup-token",
                                &[peer],
                            ),
                            false,
                        )
                    }
                    Some("announce_peer") => {
                        assert_eq!(get_peers_count, 1, "announce should reuse one lookup");
                        let args = query.a.as_ref().expect("announce_peer arguments");
                        assert_eq!(
                            args.dict_get(b"token").and_then(|value| value.as_bytes()),
                            Some(&b"lookup-token"[..])
                        );
                        assert_eq!(
                            args.dict_get(b"port").and_then(|value| value.as_int()),
                            Some(i64::from(ANNOUNCE_PORT))
                        );
                        (
                            DhtMessageBuilder::announce_peer_response(&query.t, &node_id),
                            true,
                        )
                    }
                    method => panic!("unexpected DHT query: {method:?}"),
                };
                let encoded = response.encode();
                responder
                    .send_to(&encoded, from)
                    .await
                    .expect("responder should return DHT response");
                if announced {
                    break;
                }
            }
        });

        let engine = DhtEngine::start(DhtEngineConfig {
            query_timeout: Duration::from_millis(500),
            ..DhtEngineConfig::local()
        })
        .await
        .expect("local DHT engine should start");
        engine.add_node(responder_addr).await;
        let mut engines = DhtEngineSet::default();
        engines.insert(Arc::clone(&engine));

        let result = engines
            .find_peers_and_announce(&INFO_HASH, ANNOUNCE_PORT)
            .await
            .expect("family aggregator should complete lookup and announce");
        assert_eq!(result.peers, vec![peer]);
        assert_eq!(result.nodes_contacted, 1);

        responder_task.await.expect("DHT responder should finish");
        engine.shutdown_async().await;
    }
}
