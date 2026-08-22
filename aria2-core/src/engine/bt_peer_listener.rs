//! Incoming BitTorrent peer listener and storage admission.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock, Weak};

use rand::seq::SliceRandom;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::bt_peer_storage::{DefaultPeerStorage, PeerEntry};

/// A successfully admitted incoming peer.
pub struct IncomingPeer {
    pub connection: aria2_protocol::bittorrent::peer::incoming::IncomingConnection,
    pub endpoint: SocketAddr,
}

struct SharedRoute {
    id: u64,
    local_peer_id: [u8; 20],
    caretaker_id: u64,
    max_peers: usize,
    peer_storage: Arc<Mutex<DefaultPeerStorage>>,
    sender: mpsc::Sender<IncomingPeer>,
    crypto_policy: aria2_protocol::bittorrent::peer::incoming::IncomingCryptoPolicy,
}

struct SharedListenerState {
    listener: Option<Arc<TcpListener>>,
    local_addr: Option<SocketAddr>,
    next_route_id: u64,
}

/// Configuration for one torrent route on the process-level listener.
pub struct BtPeerRouteConfig {
    pub bind_ip: IpAddr,
    pub ports: Vec<u16>,
    pub info_hash: [u8; 20],
    pub local_peer_id: [u8; 20],
    pub caretaker_id: u64,
    pub max_peers: usize,
    pub peer_storage: Arc<Mutex<DefaultPeerStorage>>,
    pub crypto_policy: aria2_protocol::bittorrent::peer::incoming::IncomingCryptoPolicy,
}

/// Process-level BitTorrent listener and info-hash router.
///
/// The socket is created once for an engine and routes incoming plain
/// handshakes by their torrent info-hash. A route handle owns registration for
/// one task and unregisters it on drop, so completed downloads cannot receive
/// new peers.
#[derive(Clone)]
pub struct BtPeerListenerManager {
    state: Arc<tokio::sync::Mutex<SharedListenerState>>,
    routes: Arc<RwLock<HashMap<[u8; 20], SharedRoute>>>,
    shutdown: CancellationToken,
}

/// RAII registration for one torrent on [`BtPeerListenerManager`].
pub struct BtPeerRouteHandle {
    routes: Weak<RwLock<HashMap<[u8; 20], SharedRoute>>>,
    info_hash: [u8; 20],
    id: u64,
}

impl BtPeerListenerManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(tokio::sync::Mutex::new(SharedListenerState {
                listener: None,
                local_addr: None,
                next_route_id: 1,
            })),
            routes: Arc::new(RwLock::new(HashMap::new())),
            shutdown: CancellationToken::new(),
        }
    }

    /// Bind the process listener if necessary and register a route with its
    /// incoming handshake policy.
    pub async fn register(
        &self,
        config: BtPeerRouteConfig,
    ) -> io::Result<(u16, mpsc::Receiver<IncomingPeer>, BtPeerRouteHandle)> {
        let mut state = self.state.lock().await;
        if let Some(listener) = state.listener.as_ref() {
            let port = listener.local_addr()?.port();
            drop(state);
            return self.insert_route(config, port);
        }

        let listener = bind_ports(config.bind_ip, config.ports.clone()).await?;
        let local_addr = listener.local_addr()?;
        let listener = Arc::new(listener);
        state.local_addr = Some(local_addr);
        state.listener = Some(Arc::clone(&listener));
        drop(state);

        let routes = Arc::clone(&self.routes);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move { run_shared_listener(listener, routes, shutdown).await });

        self.insert_route(config, local_addr.port())
    }

    pub async fn local_addr(&self) -> Option<SocketAddr> {
        self.state.lock().await.local_addr
    }

    /// Stop accepting new peers and release the process listener.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let mut state = self.state.lock().await;
        state.listener.take();
        state.local_addr = None;
    }

    fn insert_route(
        &self,
        config: BtPeerRouteConfig,
        port: u16,
    ) -> io::Result<(u16, mpsc::Receiver<IncomingPeer>, BtPeerRouteHandle)> {
        let BtPeerRouteConfig {
            info_hash,
            local_peer_id,
            caretaker_id,
            max_peers,
            peer_storage,
            crypto_policy,
            ..
        } = config;
        let (sender, receiver) = mpsc::channel(max_peers.max(1));
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| io::Error::other("BitTorrent listener state is busy"))?;
        let id = state.next_route_id;
        state.next_route_id = state.next_route_id.wrapping_add(1);
        drop(state);

        let mut routes = self
            .routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if routes.contains_key(&info_hash) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "BitTorrent info-hash route is already registered",
            ));
        }
        routes.insert(
            info_hash,
            SharedRoute {
                id,
                local_peer_id,
                caretaker_id,
                max_peers,
                peer_storage,
                sender,
                crypto_policy,
            },
        );
        drop(routes);

        Ok((
            port,
            receiver,
            BtPeerRouteHandle {
                routes: Arc::downgrade(&self.routes),
                info_hash,
                id,
            },
        ))
    }
}

impl Default for BtPeerListenerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BtPeerListenerManager {
    fn drop(&mut self) {
        // Directly constructed commands own their manager. Cancelling here
        // lets the accept task release its socket when the last Arc goes
        // away; DownloadEngine still performs its explicit async shutdown.
        self.shutdown.cancel();
    }
}

impl Drop for BtPeerRouteHandle {
    fn drop(&mut self) {
        let Some(routes) = self.routes.upgrade() else {
            return;
        };
        let mut routes = routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if routes
            .get(&self.info_hash)
            .is_some_and(|route| route.id == self.id)
        {
            routes.remove(&self.info_hash);
        }
    }
}

async fn bind_ports(
    bind_ip: IpAddr,
    ports: impl IntoIterator<Item = u16>,
) -> io::Result<TcpListener> {
    let mut ports = ports.into_iter().collect::<Vec<_>>();
    ports.shuffle(&mut rand::thread_rng());
    let mut last_error = None;
    for port in ports {
        match TcpListener::bind(SocketAddr::new(bind_ip, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "BitTorrent listen port range is empty",
        )
    }))
}

async fn run_shared_listener(
    listener: Arc<TcpListener>,
    routes: Arc<RwLock<HashMap<[u8; 20], SharedRoute>>>,
    shutdown: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            _ = shutdown.cancelled() => break,
            result = listener.accept() => result,
        };
        let Ok((stream, endpoint)) = accepted else {
            break;
        };
        let routes = Arc::clone(&routes);
        tokio::spawn(async move {
            let known_info_hashes = {
                let routes = routes
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                routes.keys().copied().collect::<Vec<_>>()
            };
            let policies = {
                let routes = routes
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                routes
                    .iter()
                    .map(|(hash, route)| (*hash, route.crypto_policy))
                    .collect::<HashMap<_, _>>()
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
                routes.get(&info_hash).map(|route| SharedRoute {
                    id: route.id,
                    local_peer_id: route.local_peer_id,
                    caretaker_id: route.caretaker_id,
                    max_peers: route.max_peers,
                    peer_storage: Arc::clone(&route.peer_storage),
                    sender: route.sender.clone(),
                    crypto_policy: route.crypto_policy,
                })
            };
            let Some(route) = route else {
                tracing::debug!(%endpoint, "Rejected incoming peer for unknown info-hash");
                return;
            };
            let connection = match incoming.complete(route.local_peer_id).await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!(%endpoint, %error, "Incoming BitTorrent handshake failed");
                    return;
                }
            };
            tracing::debug!(%endpoint, remote_peer_id = ?connection.remote_peer_id(), "Incoming BitTorrent handshake completed");
            let admitted = {
                let mut storage = route
                    .peer_storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if route.max_peers != 0 && storage.used_peers().len() >= route.max_peers {
                    false
                } else {
                    let entry = PeerEntry::new(endpoint.ip().to_string(), endpoint.port());
                    let admitted = storage
                        .add_and_checkout_peer(entry, route.caretaker_id)
                        .is_some();
                    if admitted {
                        storage.set_peer_active(&endpoint.ip().to_string(), endpoint.port(), true);
                    }
                    admitted
                }
            };
            if !admitted {
                tracing::debug!(%endpoint, "Rejected incoming BitTorrent peer at peer-storage admission");
                return;
            }
            if route
                .sender
                .send(IncomingPeer {
                    connection,
                    endpoint,
                })
                .await
                .is_err()
            {
                tracing::debug!(%endpoint, "Incoming BitTorrent peer route receiver closed");
                route
                    .peer_storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_manager_routes_two_torrents_on_one_socket() {
        use aria2_protocol::bittorrent::message::handshake::Handshake;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let manager = BtPeerListenerManager::new();
        let storage_a = Arc::new(Mutex::new(DefaultPeerStorage::new()));
        let storage_b = Arc::new(Mutex::new(DefaultPeerStorage::new()));
        let hash_a = [11u8; 20];
        let hash_b = [22u8; 20];
        let (port, mut rx_a, route_a) = manager
            .register(BtPeerRouteConfig {
                bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                ports: vec![0],
                info_hash: hash_a,
                local_peer_id: [1; 20],
                caretaker_id: 1,
                max_peers: 4,
                peer_storage: storage_a,
                crypto_policy: Default::default(),
            })
            .await
            .unwrap();
        let (_, mut rx_b, route_b) = manager
            .register(BtPeerRouteConfig {
                bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                ports: vec![port],
                info_hash: hash_b,
                local_peer_id: [2; 20],
                caretaker_id: 2,
                max_peers: 4,
                peer_storage: storage_b,
                crypto_policy: Default::default(),
            })
            .await
            .unwrap();

        async fn connect_and_handshake(port: u16, hash: [u8; 20], peer_id: [u8; 20]) {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(&Handshake::new(&hash, &peer_id).to_bytes())
                .await
                .unwrap();
            let mut response = [0u8; 68];
            stream.read_exact(&mut response).await.unwrap();
            assert_eq!(Handshake::parse(&response).unwrap().info_hash, hash);
        }

        let first = tokio::spawn(connect_and_handshake(port, hash_a, [3; 20]));
        let incoming_a = tokio::time::timeout(std::time::Duration::from_secs(2), rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        first.await.unwrap();
        assert_eq!(incoming_a.connection.remote_peer_id(), Some([3; 20]));

        let second = tokio::spawn(connect_and_handshake(port, hash_b, [4; 20]));
        let incoming_b = tokio::time::timeout(std::time::Duration::from_secs(2), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        second.await.unwrap();
        assert_eq!(incoming_b.connection.remote_peer_id(), Some([4; 20]));
        assert!(rx_a.try_recv().is_err());

        drop(route_a);
        drop(route_b);
    }

    #[tokio::test]
    async fn shared_manager_unregisters_route_on_handle_drop() {
        let manager = BtPeerListenerManager::new();
        let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
        let hash = [33u8; 20];
        let (_, _, route) = manager
            .register(BtPeerRouteConfig {
                bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                ports: vec![0],
                info_hash: hash,
                local_peer_id: [1; 20],
                caretaker_id: 1,
                max_peers: 1,
                peer_storage: storage,
                crypto_policy: Default::default(),
            })
            .await
            .unwrap();
        drop(route);
        assert!(manager.routes.read().unwrap().get(&hash).is_none());
    }

    #[tokio::test]
    async fn shutdown_releases_the_shared_listener_socket() {
        let manager = BtPeerListenerManager::new();
        let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
        let (port, _receiver, _route) = manager
            .register(BtPeerRouteConfig {
                bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                ports: vec![0],
                info_hash: [44u8; 20],
                local_peer_id: [1; 20],
                caretaker_id: 1,
                max_peers: 1,
                peer_storage: storage,
                crypto_policy: Default::default(),
            })
            .await
            .unwrap();

        manager.shutdown().await;
        let rebound = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
                    break listener;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("listener shutdown must release the socket");
        drop(rebound);
    }
}
