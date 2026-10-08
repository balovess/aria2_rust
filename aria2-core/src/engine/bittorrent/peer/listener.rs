//! Incoming BitTorrent peer listener and storage admission.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, RwLock, Weak};

use rand::seq::SliceRandom;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::storage::DefaultPeerStorage;

/// A successfully admitted incoming peer.
pub struct IncomingPeer {
    pub connection: IncomingPeerConnection,
    pub endpoint: SocketAddr,
}

// The incoming-peer mailbox is bounded by the configured peer limit; retaining
// the TCP connection inline avoids an allocation for the common transport.
#[allow(clippy::large_enum_variant)]
pub enum IncomingPeerConnection {
    Tcp(aria2_protocol::bittorrent::peer::connection::PeerConnection),
    Utp(super::connection::UtpPeerConnection),
}

impl IncomingPeerConnection {
    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        match self {
            Self::Tcp(connection) => connection.remote_peer_id().copied(),
            Self::Utp(connection) => connection.remote_peer_id(),
        }
    }
}

pub(crate) type IncomingPeerReceiver = Arc<tokio::sync::Mutex<mpsc::Receiver<IncomingPeer>>>;

#[derive(Clone)]
struct SharedRoute {
    id: u64,
    local_peer_id: [u8; 20],
    caretaker_id: u64,
    max_peers: Arc<AtomicUsize>,
    peer_storage: Arc<Mutex<DefaultPeerStorage>>,
    sender: mpsc::Sender<IncomingPeer>,
    crypto_policy: aria2_protocol::bittorrent::peer::incoming::IncomingCryptoPolicy,
    info_hash_v2: Option<[u8; 32]>,
    dht_enabled: bool,
}

struct SharedListenerState {
    listeners: Vec<Arc<TcpListener>>,
    listener_tasks: Vec<tokio::task::JoinHandle<()>>,
    local_addr: Option<SocketAddr>,
    next_route_id: u64,
}

/// Configuration for one torrent route on the process-level listener.
pub struct BtPeerRouteConfig {
    pub bind_ip: IpAddr,
    pub ports: Vec<u16>,
    pub info_hash: [u8; 20],
    pub info_hash_v2: Option<[u8; 32]>,
    pub local_peer_id: [u8; 20],
    pub caretaker_id: u64,
    pub max_peers: usize,
    pub peer_storage: Arc<Mutex<DefaultPeerStorage>>,
    pub crypto_policy: aria2_protocol::bittorrent::peer::incoming::IncomingCryptoPolicy,
    pub dht_enabled: bool,
}

/// Process-level BitTorrent listener and info-hash router.
///
/// The socket is created once for an engine and routes incoming plain
/// handshakes by their torrent info-hash. A route handle owns registration for
/// one task and unregisters it on drop, so completed downloads cannot receive
/// new peers.
#[derive(Clone)]
pub struct BtPeerListenerManager {
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    state: Arc<tokio::sync::Mutex<SharedListenerState>>,
    routes: Arc<RwLock<HashMap<[u8; 20], SharedRoute>>>,
    utp_transports:
        Arc<tokio::sync::Mutex<HashMap<SocketAddr, super::utp_transport::UtpTransportActor>>>,
    utp_incoming_tx: mpsc::Sender<super::utp_transport::IncomingUtpConnection>,
    utp_incoming_rx: Arc<
        tokio::sync::Mutex<Option<mpsc::Receiver<super::utp_transport::IncomingUtpConnection>>>,
    >,
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
        let (utp_incoming_tx, utp_incoming_rx) = mpsc::channel(128);
        Self {
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            state: Arc::new(tokio::sync::Mutex::new(SharedListenerState {
                listeners: Vec::new(),
                listener_tasks: Vec::new(),
                local_addr: None,
                next_route_id: 1,
            })),
            routes: Arc::new(RwLock::new(HashMap::new())),
            utp_transports: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            utp_incoming_tx,
            utp_incoming_rx: Arc::new(tokio::sync::Mutex::new(Some(utp_incoming_rx))),
            shutdown: CancellationToken::new(),
        }
    }

    /// Register or reuse the process-owned uTP actor for one local endpoint.
    pub(crate) async fn register_utp_transport(
        &self,
        address: SocketAddr,
    ) -> io::Result<super::utp_transport::UtpTransportHandle> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.shutdown.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "BitTorrent peer listener is shutting down",
            ));
        }
        let transport = {
            let mut transports = self.utp_transports.lock().await;
            if self.shutdown.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "BitTorrent peer listener is shutting down",
                ));
            }
            if let Some(transport) = transports.get(&address) {
                transport.handle()
            } else {
                let transport = super::utp_transport::UtpTransportActor::bind(
                    address,
                    self.utp_incoming_tx.clone(),
                    self.shutdown.clone(),
                )?;
                let handle = transport.handle();
                transports.insert(address, transport);
                handle
            }
        };

        if let Some(receiver) = self.utp_incoming_rx.lock().await.take() {
            let task = tokio::spawn(incoming::run_shared_utp_listener(
                receiver,
                Arc::clone(&self.routes),
                self.shutdown.clone(),
            ));
            self.state.lock().await.listener_tasks.push(task);
        }
        Ok(transport)
    }

    /// Bind the process listener if necessary and register a route with its
    /// incoming handshake policy.
    pub async fn register(
        &self,
        config: BtPeerRouteConfig,
    ) -> io::Result<(u16, mpsc::Receiver<IncomingPeer>, BtPeerRouteHandle)> {
        let max_peers = Arc::new(AtomicUsize::new(config.max_peers));
        self.register_with_max_peers(config, max_peers).await
    }

    pub(crate) async fn register_with_max_peers(
        &self,
        config: BtPeerRouteConfig,
        max_peers: Arc<AtomicUsize>,
    ) -> io::Result<(u16, mpsc::Receiver<IncomingPeer>, BtPeerRouteHandle)> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.shutdown.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "BitTorrent peer listener is shutting down",
            ));
        }
        let mut state = self.state.lock().await;
        if let Some(listener) = state.listeners.first() {
            let port = listener.local_addr()?.port();
            drop(state);
            return self.insert_route(config, port, max_peers);
        }

        let listener = bind_ports(config.bind_ip, config.ports.clone()).await?;
        let local_addr = listener.local_addr()?;
        let mut listeners = vec![Arc::new(listener)];
        if config.bind_ip == IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED) {
            // aria2_original starts one peer listener per address family. On
            // Windows, an IPv6 socket is commonly v6-only, so an IPv4 peer
            // returned by a tracker would otherwise receive ECONNREFUSED.
            if let Ok(listener) = TcpListener::bind(SocketAddr::new(
                IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                local_addr.port(),
            ))
            .await
            {
                listeners.push(Arc::new(listener));
            }
        }
        state.local_addr = Some(local_addr);
        state.listeners = listeners.clone();
        drop(state);

        let routes = Arc::clone(&self.routes);
        let shutdown = self.shutdown.clone();
        let mut listener_tasks = Vec::with_capacity(listeners.len());
        for listener in listeners {
            let routes = Arc::clone(&routes);
            let shutdown = shutdown.clone();
            listener_tasks.push(tokio::spawn(async move {
                incoming::run_shared_listener(listener, routes, shutdown).await;
            }));
        }
        self.state
            .lock()
            .await
            .listener_tasks
            .extend(listener_tasks);

        self.insert_route(config, local_addr.port(), max_peers)
    }

    pub async fn local_addr(&self) -> Option<SocketAddr> {
        self.state.lock().await.local_addr
    }

    /// Stop accepting new peers and release the process listener.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let _lifecycle = self.lifecycle.lock().await;
        let listener_tasks = {
            let mut state = self.state.lock().await;
            state.listeners.clear();
            state.local_addr = None;
            std::mem::take(&mut state.listener_tasks)
        };
        for task in listener_tasks {
            if let Err(error) = task.await {
                tracing::warn!(%error, "BitTorrent listener task failed during shutdown");
            }
        }
        let transports = {
            let mut transports = self.utp_transports.lock().await;
            std::mem::take(&mut *transports)
        };
        for (_, transport) in transports {
            if let Err(error) = transport.join().await {
                tracing::warn!(%error, "uTP transport actor task failed during shutdown");
            }
        }
    }

    fn insert_route(
        &self,
        config: BtPeerRouteConfig,
        port: u16,
        max_peers_state: Arc<AtomicUsize>,
    ) -> io::Result<(u16, mpsc::Receiver<IncomingPeer>, BtPeerRouteHandle)> {
        let BtPeerRouteConfig {
            info_hash,
            local_peer_id,
            caretaker_id,
            max_peers,
            peer_storage,
            crypto_policy,
            info_hash_v2,
            dht_enabled,
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
                max_peers: max_peers_state,
                peer_storage,
                sender,
                crypto_policy,
                info_hash_v2,
                dht_enabled,
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
        // All manager clones share the same listener state. Only the last
        // owner should stop the accept tasks; dropping an intermediate clone
        // must not disable the listener still used by another owner.
        if Arc::strong_count(&self.state) == 1 {
            self.shutdown.cancel();
        }
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

mod incoming;

#[cfg(test)]
mod tests;
