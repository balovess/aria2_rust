//! Incoming BitTorrent peer listener and storage admission.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::bt_peer_storage::{DefaultPeerStorage, PeerEntry};

/// A successfully admitted incoming peer.
pub struct IncomingPeer {
    pub connection: PeerConnection,
    pub endpoint: SocketAddr,
}

enum ListenerError {
    Accept(io::Error),
    Rejected(String),
}

/// Accepts incoming TCP peers for one torrent and admits them to storage.
pub struct BtPeerListener {
    listener: TcpListener,
    info_hash: [u8; 20],
    local_peer_id: [u8; 20],
    caretaker_id: u64,
    max_peers: usize,
    peer_storage: Arc<Mutex<DefaultPeerStorage>>,
}

impl BtPeerListener {
    pub async fn bind(
        bind_addr: SocketAddr,
        info_hash: [u8; 20],
        local_peer_id: [u8; 20],
        caretaker_id: u64,
        max_peers: usize,
        peer_storage: Arc<Mutex<DefaultPeerStorage>>,
    ) -> std::io::Result<Self> {
        Self::bind_ports(
            bind_addr.ip(),
            std::iter::once(bind_addr.port()),
            info_hash,
            local_peer_id,
            caretaker_id,
            max_peers,
            peer_storage,
        )
        .await
    }

    pub async fn bind_ports(
        bind_ip: IpAddr,
        ports: impl IntoIterator<Item = u16>,
        info_hash: [u8; 20],
        local_peer_id: [u8; 20],
        caretaker_id: u64,
        max_peers: usize,
        peer_storage: Arc<Mutex<DefaultPeerStorage>>,
    ) -> io::Result<Self> {
        let mut last_error = None;
        for port in ports {
            match TcpListener::bind(SocketAddr::new(bind_ip, port)).await {
                Ok(listener) => {
                    return Ok(Self {
                        listener,
                        info_hash,
                        local_peer_id,
                        caretaker_id,
                        max_peers,
                        peer_storage,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    last_error = Some(error);
                }
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

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn return_peer(&self, endpoint: SocketAddr) {
        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        storage.return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
    }

    /// Run the listener and deliver admitted peers to the download loop.
    pub fn spawn(
        self,
        capacity: usize,
    ) -> (mpsc::Receiver<IncomingPeer>, tokio::task::JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let task = tokio::spawn(async move {
            loop {
                match self.accept_one_internal().await {
                    Ok(peer) => {
                        if let Err(error) = sender.send(peer).await {
                            let endpoint = error.0.endpoint;
                            self.return_peer(endpoint);
                            tracing::debug!(%endpoint, "Incoming peer receiver closed; ownership returned");
                            break;
                        }
                    }
                    Err(ListenerError::Accept(error)) => {
                        tracing::debug!(%error, "BitTorrent listener stopped after accept failure");
                        break;
                    }
                    Err(ListenerError::Rejected(error)) => {
                        tracing::debug!(%error, "Rejected incoming BitTorrent peer");
                    }
                }
            }
        });
        (receiver, task)
    }

    pub async fn accept_one(&self) -> Result<IncomingPeer, String> {
        self.accept_one_internal()
            .await
            .map_err(|error| match error {
                ListenerError::Accept(error) => error.to_string(),
                ListenerError::Rejected(error) => error,
            })
    }

    async fn accept_one_internal(&self) -> Result<IncomingPeer, ListenerError> {
        let (stream, endpoint) = self
            .listener
            .accept()
            .await
            .map_err(ListenerError::Accept)?;
        self.admit_stream(stream, endpoint)
            .await
            .map_err(ListenerError::Rejected)
    }

    async fn admit_stream(
        &self,
        stream: TcpStream,
        endpoint: SocketAddr,
    ) -> Result<IncomingPeer, String> {
        let connection =
            PeerConnection::from_incoming_stream(stream, &self.info_hash, &self.local_peer_id)
                .await?;

        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.max_peers != 0 && storage.used_peers().len() >= self.max_peers {
            return Err(format!("peer limit reached for {endpoint}"));
        }

        let entry = PeerEntry::new(endpoint.ip().to_string(), endpoint.port());
        if storage
            .add_and_checkout_peer(entry, self.caretaker_id)
            .is_none()
        {
            return Err(format!("peer rejected by storage: {endpoint}"));
        }
        storage.set_peer_active(&endpoint.ip().to_string(), endpoint.port(), true);
        drop(storage);

        Ok(IncomingPeer {
            connection,
            endpoint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_uses_an_ephemeral_port() {
        let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
        let listener = BtPeerListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            [1; 20],
            [2; 20],
            7,
            4,
            storage,
        )
        .await
        .unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn spawn_task_can_be_aborted_without_an_incoming_connection() {
        let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
        let listener = BtPeerListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            [1; 20],
            [2; 20],
            7,
            4,
            Arc::clone(&storage),
        )
        .await
        .unwrap();
        let addr = listener.local_addr().unwrap();
        let (receiver, task) = listener.spawn(1);
        drop(receiver);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let rebound = TcpListener::bind(addr).await;
        assert!(
            rebound.is_ok(),
            "listener socket was not released after abort"
        );
    }

    #[tokio::test]
    async fn accepts_handshakes_and_admits_endpoint() {
        use aria2_protocol::bittorrent::message::handshake::Handshake;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let info_hash = [1; 20];
        let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
        let listener = BtPeerListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            info_hash,
            [2; 20],
            7,
            4,
            Arc::clone(&storage),
        )
        .await
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(&Handshake::new(&info_hash, &[3; 20]).to_bytes())
                .await
                .unwrap();
            let mut response = [0; 68];
            stream.read_exact(&mut response).await.unwrap();
            Handshake::parse(&response).unwrap()
        });

        let incoming = listener.accept_one().await.unwrap();
        assert_eq!(incoming.endpoint.ip().to_string(), "127.0.0.1");
        assert_eq!(incoming.connection.remote_peer_id, Some([3; 20]));
        let response = client.await.unwrap();
        assert_eq!(response.info_hash, info_hash);
        assert_eq!(storage.lock().unwrap().used_peers().len(), 1);
    }
}
