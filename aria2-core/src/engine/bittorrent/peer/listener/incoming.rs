//! Bounded lifecycle for incoming BitTorrent TCP and uTP peer handshakes.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use super::super::storage::{DefaultPeerStorage, PeerEntry};
use super::{IncomingPeer, IncomingPeerConnection, SharedRoute};

pub(super) async fn run_shared_listener(
    listener: Arc<TcpListener>,
    routes: Arc<RwLock<HashMap<[u8; 20], SharedRoute>>>,
    shutdown: CancellationToken,
) {
    enum Event {
        Shutdown,
        Accepted(io::Result<(tokio::net::TcpStream, SocketAddr)>),
        ConnectionFinished(Result<(), JoinError>),
    }

    let connection_shutdown = shutdown.child_token();
    let mut connections = JoinSet::new();
    loop {
        let event = tokio::select! {
            _ = shutdown.cancelled() => Event::Shutdown,
            result = listener.accept() => Event::Accepted(result),
            result = connections.join_next(), if !connections.is_empty() => {
                Event::ConnectionFinished(result.expect("non-empty JoinSet has a task"))
            }
        };
        match event {
            Event::Shutdown => break,
            Event::Accepted(Ok((stream, endpoint))) => {
                let routes = Arc::clone(&routes);
                let connection_shutdown = connection_shutdown.clone();
                connections.spawn(async move {
                    tokio::select! {
                        _ = connection_shutdown.cancelled() => {},
                        _ = process_incoming_tcp_peer(stream, endpoint, routes) => {},
                    }
                });
            }
            Event::Accepted(Err(error)) => {
                tracing::debug!(%error, "BitTorrent listener accept failed");
                break;
            }
            Event::ConnectionFinished(Err(error)) if !error.is_cancelled() => {
                tracing::warn!(%error, "Incoming BitTorrent TCP task failed");
            }
            Event::ConnectionFinished(_) => {}
        }
    }

    connection_shutdown.cancel();
    drain_connections(
        connections,
        "Incoming BitTorrent TCP task failed during shutdown",
    )
    .await;
}

async fn process_incoming_tcp_peer(
    stream: tokio::net::TcpStream,
    endpoint: SocketAddr,
    routes: Arc<RwLock<HashMap<[u8; 20], SharedRoute>>>,
) {
    let endpoint = normalize_peer_endpoint(endpoint);
    let (known_info_hashes, policies) = {
        let routes = routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let known_info_hashes = routes.keys().copied().collect::<Vec<_>>();
        let policies = routes
            .iter()
            .map(|(hash, route)| (*hash, route.crypto_policy))
            .collect::<HashMap<_, _>>();
        (known_info_hashes, policies)
    };
    let incoming = match aria2_protocol::bittorrent::peer::incoming::receive_with_policies(
        stream,
        &known_info_hashes,
        &policies,
    )
    .await
    {
        Ok(incoming) => incoming,
        Err(error) => {
            tracing::debug!(%endpoint, %error, "Rejected incoming BitTorrent handshake");
            return;
        }
    };
    tracing::debug!(%endpoint, info_hash = %hex::encode(incoming.info_hash()), "Incoming BitTorrent handshake accepted");
    let info_hash = *incoming.info_hash();
    let route = {
        let routes = routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        routes.get(&info_hash).cloned()
    };
    let Some(route) = route else {
        tracing::debug!(%endpoint, "Rejected incoming peer for unknown info-hash");
        return;
    };
    let connection = match incoming
        .complete(route.local_peer_id, route.info_hash_v2, route.dht_enabled)
        .await
    {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(%endpoint, %error, "Incoming BitTorrent handshake failed");
            return;
        }
    };
    tracing::debug!(%endpoint, remote_peer_id = ?connection.remote_peer_id(), "Incoming BitTorrent handshake completed");
    admit_incoming_peer(&route, IncomingPeerConnection::Tcp(connection), endpoint).await;
}

pub(super) async fn run_shared_utp_listener(
    mut incoming: mpsc::Receiver<super::super::utp_transport::IncomingUtpConnection>,
    routes: Arc<RwLock<HashMap<[u8; 20], SharedRoute>>>,
    shutdown: CancellationToken,
) {
    enum Event {
        Shutdown,
        Incoming(Option<super::super::utp_transport::IncomingUtpConnection>),
        ConnectionFinished(Result<(), JoinError>),
    }

    let connection_shutdown = shutdown.child_token();
    let mut connections = JoinSet::new();
    loop {
        let event = tokio::select! {
            _ = shutdown.cancelled() => Event::Shutdown,
            incoming = incoming.recv() => Event::Incoming(incoming),
            result = connections.join_next(), if !connections.is_empty() => {
                Event::ConnectionFinished(result.expect("non-empty JoinSet has a task"))
            }
        };
        match event {
            Event::Shutdown | Event::Incoming(None) => break,
            Event::Incoming(Some(incoming)) => {
                let routes = Arc::clone(&routes);
                let connection_shutdown = connection_shutdown.clone();
                connections.spawn(async move {
                    tokio::select! {
                        _ = connection_shutdown.cancelled() => {},
                        _ = route_incoming_utp_peer(incoming, routes) => {},
                    }
                });
            }
            Event::ConnectionFinished(Err(error)) if !error.is_cancelled() => {
                tracing::warn!(%error, "Incoming BitTorrent uTP task failed");
            }
            Event::ConnectionFinished(_) => {}
        }
    }

    connection_shutdown.cancel();
    drain_connections(
        connections,
        "Incoming BitTorrent uTP task failed during shutdown",
    )
    .await;
}

async fn drain_connections(mut connections: JoinSet<()>, failure_message: &'static str) {
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "{failure_message}");
        }
    }
}

fn normalize_peer_endpoint(endpoint: SocketAddr) -> SocketAddr {
    match endpoint {
        SocketAddr::V6(address) => address
            .ip()
            .to_ipv4()
            .map(|ip| SocketAddr::new(IpAddr::V4(ip), address.port()))
            .unwrap_or(endpoint),
        SocketAddr::V4(_) => endpoint,
    }
}

async fn route_incoming_utp_peer(
    incoming: super::super::utp_transport::IncomingUtpConnection,
    routes: Arc<RwLock<HashMap<[u8; 20], SharedRoute>>>,
) {
    let endpoint = normalize_peer_endpoint(incoming.endpoint);
    let (mut connection, handshake) =
        match super::super::connection::UtpPeerConnection::receive_incoming_handshake(
            incoming.connection,
            endpoint,
            std::time::Duration::from_secs(30),
        )
        .await
        {
            Ok(incoming) => incoming,
            Err(error) => {
                tracing::debug!(%endpoint, %error, "Rejected incoming uTP BitTorrent handshake");
                return;
            }
        };
    let info_hash = handshake.info_hash;
    let route = routes
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&info_hash)
        .cloned();
    let Some(route) = route else {
        tracing::debug!(%endpoint, info_hash = %hex::encode(info_hash), "Rejected incoming uTP peer for unknown info-hash");
        return;
    };
    if route.crypto_policy.reject_plain {
        tracing::debug!(%endpoint, "Rejected plaintext uTP peer because encryption is required");
        return;
    }
    if let Err(error) = connection
        .complete_incoming_handshake(
            &handshake,
            &route.local_peer_id,
            route.info_hash_v2,
            route.dht_enabled,
        )
        .await
    {
        tracing::debug!(%endpoint, %error, "Failed to complete incoming uTP handshake");
        return;
    }

    admit_incoming_peer(&route, IncomingPeerConnection::Utp(connection), endpoint).await;
}

async fn admit_incoming_peer(
    route: &SharedRoute,
    connection: IncomingPeerConnection,
    endpoint: SocketAddr,
) {
    let admitted = {
        let mut storage = route
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let max_peers = route.max_peers.load(Ordering::Acquire);
        if max_peers != 0 && storage.used_peers().len() >= max_peers {
            false
        } else {
            let ip = endpoint.ip().to_string();
            let admitted = storage
                .add_and_checkout_peer(
                    PeerEntry::new(ip.clone(), endpoint.port()),
                    route.caretaker_id,
                )
                .is_some();
            if admitted {
                storage.set_peer_active(&ip, endpoint.port(), true);
            }
            admitted
        }
    };
    if !admitted {
        tracing::debug!(%endpoint, "Rejected incoming BitTorrent peer at peer-storage admission");
        return;
    }

    let mut admission = PeerAdmissionGuard::new(Arc::clone(&route.peer_storage), endpoint);
    if route
        .sender
        .send(IncomingPeer {
            connection,
            endpoint,
        })
        .await
        .is_ok()
    {
        admission.commit();
    } else {
        tracing::debug!(%endpoint, "Incoming BitTorrent peer route receiver closed");
    }
}

struct PeerAdmissionGuard {
    peer_storage: Arc<Mutex<DefaultPeerStorage>>,
    endpoint: SocketAddr,
    armed: bool,
}

impl PeerAdmissionGuard {
    fn new(peer_storage: Arc<Mutex<DefaultPeerStorage>>, endpoint: SocketAddr) -> Self {
        Self {
            peer_storage,
            endpoint,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for PeerAdmissionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .return_peer_by_endpoint(&self.endpoint.ip().to_string(), self.endpoint.port());
        }
    }
}
