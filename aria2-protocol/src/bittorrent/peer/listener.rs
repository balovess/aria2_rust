//! BitTorrent incoming TCP connection listener.
//!
//! Equivalent to C++ `PeerListenCommand`. Binds a TCP socket and accepts
//! incoming peer connections, forwarding them via a channel to the download
//! engine for MSE handshake and session assignment.
//!
//! # C++ Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/PeerListenCommand.h` / `src/PeerListenCommand.cc`
//!
//! The C++ version re-adds itself to the command queue after each iteration
//! (cooperative multitasking). The Rust version uses tokio's async accept
//! loop with a oneshot shutdown signal instead.

use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Maximum number of connections to accept per accept cycle.
///
/// Matches the C++ constant: `for (int i = 0; i < 3 && socket_->isReadable(0); ++i)`.
const MAX_ACCEPT_PER_CYCLE: usize = 3;

/// Channel message for notifying the engine about an accepted peer.
///
/// The engine receives this and spawns an MSE handshake handler
/// (equivalent to C++ `ReceiverMSEHandshakeCommand`).
#[derive(Debug)]
pub struct AcceptedPeer {
    /// The accepted TCP stream.
    pub stream: tokio::net::TcpStream,
    /// Remote address of the accepted peer.
    pub addr: SocketAddr,
}

/// BitTorrent TCP listener for incoming peer connections.
///
/// Binds to the configured port and accepts incoming connections,
/// sending them via a channel to the download engine for MSE
/// handshake and session assignment.
///
/// This is the Rust equivalent of C++ `PeerListenCommand`.
pub struct PeerListener {
    /// The TCP listener socket.
    listener: TcpListener,
    /// Sender for accepted peer notifications.
    tx: mpsc::Sender<AcceptedPeer>,
    /// The port actually bound (may differ from requested port if 0).
    port: u16,
}

impl PeerListener {
    /// Bind a TCP listener on the specified port.
    ///
    /// If `port` is 0, the OS assigns a random available port.
    /// Returns the listener and the actual bound port.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if binding fails (e.g. port already in use).
    pub async fn bind(port: u16, tx: mpsc::Sender<AcceptedPeer>) -> std::io::Result<Self> {
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(&addr).await?;
        let actual_port = listener.local_addr()?.port();
        info!(
            port = actual_port,
            "BitTorrent: listening for incoming peer connections"
        );
        Ok(Self {
            listener,
            tx,
            port: actual_port,
        })
    }

    /// Run the accept loop, accepting connections and sending them
    /// through the channel. Returns when the shutdown signal is received
    /// or the receiver half of the channel is dropped.
    ///
    /// Each accept cycle processes up to [`MAX_ACCEPT_PER_CYCLE`] connections,
    /// matching the C++ behavior of `PeerListenCommand::execute()`.
    /// After the first connection arrives (via `select!`), additional
    /// pending connections are drained in a tight loop with a micro-timeout,
    /// mirroring the C++ `socket_->isReadable(0)` non-blocking check.
    pub async fn run(self, mut shutdown: tokio::sync::oneshot::Receiver<()>) {
        info!(port = self.port, "Peer listener accept loop started");
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    info!(port = self.port, "Peer listener shutting down");
                    break;
                }
                result = self.listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            debug!(addr = %addr, "Accepted incoming peer connection");
                            let peer = AcceptedPeer { stream, addr };
                            if self.tx.send(peer).await.is_err() {
                                warn!("Peer receiver dropped, stopping listener");
                                break;
                            }
                            // Drain additional pending connections up to the
                            // batch limit, matching C++ PeerListenCommand which
                            // accepts up to 3 per execute() cycle.
                            self.drain_pending().await;
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to accept incoming connection");
                        }
                    }
                }
            }
        }
        info!(port = self.port, "Peer listener accept loop exited");
    }

    /// Attempt to accept additional pending connections up to the batch limit.
    ///
    /// Uses a brief timeout per attempt to avoid blocking when no more
    /// connections are waiting. This mirrors the C++ behavior where
    /// `socket_->isReadable(0)` checks for immediately available data
    /// without blocking.
    async fn drain_pending(&self) {
        for _ in 1..MAX_ACCEPT_PER_CYCLE {
            match tokio::time::timeout(
                std::time::Duration::from_micros(500),
                self.listener.accept(),
            )
            .await
            {
                Ok(Ok((stream, addr))) => {
                    debug!(addr = %addr, "Accepted incoming peer connection (drain)");
                    let peer = AcceptedPeer { stream, addr };
                    if self.tx.send(peer).await.is_err() {
                        warn!("Peer receiver dropped during drain");
                        return;
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "Failed to accept incoming connection (drain)");
                    return;
                }
                Err(_) => return, // Timeout, no more pending connections
            }
        }
    }

    /// Returns the bound port number.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns the local address the listener is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_bind_port_zero_gets_assigned() {
        let (tx, _rx) = mpsc::channel::<AcceptedPeer>(16);
        let listener = PeerListener::bind(0, tx).await;
        assert!(listener.is_ok(), "bind(0) should succeed");
        let listener = listener.unwrap();
        assert_ne!(listener.port(), 0, "OS should assign a non-zero port");
        assert!(listener.local_addr().is_ok());
    }

    #[tokio::test]
    async fn test_bind_specific_port() {
        // Bind port 0 first to find an available port, then rebind to it.
        let (tx, _rx) = mpsc::channel::<AcceptedPeer>(16);
        let tmp = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let available_port = tmp.local_addr().unwrap().port();
        drop(tmp);

        let listener = PeerListener::bind(available_port, tx).await;
        assert!(listener.is_ok(), "bind to available port should succeed");
        assert_eq!(listener.unwrap().port(), available_port);
    }

    #[tokio::test]
    async fn test_accepted_peer_carries_addr() {
        let (tx, mut rx) = mpsc::channel::<AcceptedPeer>(16);
        let listener = PeerListener::bind(0, tx).await.unwrap();
        let bound_port = listener.port();

        // Spawn the listener run loop in a background task with a shutdown
        // signal so we can stop it cleanly after the test.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let listener_task = tokio::spawn(async move {
            listener.run(shutdown_rx).await;
        });

        // Connect to the listener from another task.
        // Use 127.0.0.1 instead of 0.0.0.0 (Windows rejects connect to 0.0.0.0).
        let connect_addr: SocketAddr = format!("127.0.0.1:{}", bound_port)
            .parse()
            .expect("valid connect address");
        let connect_task = tokio::spawn(async move {
            let _stream = tokio::net::TcpStream::connect(connect_addr).await.unwrap();
        });

        // Accept one connection via the channel.
        let accepted = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for accepted peer")
            .expect("channel closed unexpectedly");

        assert_eq!(
            accepted.addr.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "accepted peer should be from localhost"
        );

        // Shut down the listener and wait for cleanup.
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), listener_task).await;
        connect_task.await.unwrap();
    }
}
