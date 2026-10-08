//! Process-owned uTP socket actor.
//!
//! One actor owns the UDP socket and all uTP protocol state for a bound local
//! endpoint. Peer actors use bounded commands and per-connection receive
//! queues; they never lock or poll `UtpSocket` themselves.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aria2_protocol::bittorrent::utp::{ConnectionState, UtpSocket};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

const COMMAND_CAPACITY: usize = 256;
const RECEIVE_CAPACITY: usize = 32;
const MAX_PENDING_SEND_BYTES: usize = 256 * 1024;

pub(crate) struct IncomingUtpConnection {
    pub(crate) connection: UtpConnectionHandle,
    pub(crate) endpoint: SocketAddr,
}

#[derive(Clone)]
pub(crate) struct UtpTransportHandle {
    command_tx: mpsc::Sender<Command>,
}

pub(crate) struct UtpConnectionHandle {
    id: u16,
    command_tx: mpsc::Sender<Command>,
    receive_rx: mpsc::Receiver<Vec<u8>>,
    state_rx: watch::Receiver<ConnectionState>,
    close_requested: Arc<AtomicBool>,
    close_notify: Arc<Notify>,
}

enum Command {
    Connect {
        endpoint: SocketAddr,
        reply: oneshot::Sender<Result<UtpConnectionHandle, String>>,
    },
    Send {
        id: u16,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Close {
        id: u16,
        reply: Option<oneshot::Sender<Result<(), String>>>,
    },
}

struct PendingSend {
    bytes: Vec<u8>,
    offset: usize,
    reply: oneshot::Sender<Result<(), String>>,
}

struct ActorConnection {
    receive_tx: mpsc::Sender<Vec<u8>>,
    state_tx: watch::Sender<ConnectionState>,
    pending_sends: VecDeque<PendingSend>,
    pending_send_bytes: usize,
    rejected: bool,
    close_requested: Arc<AtomicBool>,
}

impl UtpTransportHandle {
    pub(crate) fn bind(
        address: SocketAddr,
        incoming_tx: mpsc::Sender<IncomingUtpConnection>,
        shutdown: CancellationToken,
    ) -> std::io::Result<Self> {
        let socket = UtpSocket::bind_addr(address).map_err(std::io::Error::other)?;
        let readiness = socket.readiness_socket().map_err(std::io::Error::other)?;
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let close_notify = Arc::new(Notify::new());
        tokio::spawn(run_actor(
            socket,
            readiness,
            command_rx,
            command_tx.clone(),
            incoming_tx,
            Arc::clone(&close_notify),
            shutdown,
        ));
        Ok(Self { command_tx })
    }

    pub(crate) async fn connect(
        &self,
        endpoint: SocketAddr,
    ) -> Result<UtpConnectionHandle, String> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(Command::Connect { endpoint, reply })
            .await
            .map_err(|_| "uTP transport actor stopped".to_string())?;
        response
            .await
            .map_err(|_| "uTP transport actor dropped a connect response".to_string())?
    }
}

impl UtpConnectionHandle {
    pub(crate) async fn wait_established(&mut self, timeout: Duration) -> Result<(), String> {
        tokio::time::timeout(timeout, async {
            loop {
                match *self.state_rx.borrow_and_update() {
                    ConnectionState::Established => return Ok(()),
                    ConnectionState::Closed
                    | ConnectionState::Closing
                    | ConnectionState::FinWait
                    | ConnectionState::TimeWait => {
                        return Err("uTP connection closed during setup".to_string());
                    }
                    ConnectionState::SynSent | ConnectionState::SynReceived => {}
                }
                self.state_rx
                    .changed()
                    .await
                    .map_err(|_| "uTP transport actor stopped".to_string())?;
            }
        })
        .await
        .map_err(|_| "uTP connection setup timed out".to_string())?
    }

    pub(crate) async fn send(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() > MAX_PENDING_SEND_BYTES {
            return Err("uTP per-peer send queue is full".to_string());
        }
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(Command::Send {
                id: self.id,
                bytes: bytes.to_vec(),
                reply,
            })
            .await
            .map_err(|_| "uTP transport actor stopped".to_string())?;
        response
            .await
            .map_err(|_| "uTP transport actor dropped a send response".to_string())?
    }

    pub(crate) async fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            match self.receive_rx.try_recv() {
                Ok(bytes) => return Ok(Some(bytes)),
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(None),
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            if is_terminal(*self.state_rx.borrow()) {
                return Ok(None);
            }
            tokio::select! {
                bytes = self.receive_rx.recv() => return Ok(bytes),
                changed = self.state_rx.changed() => {
                    if changed.is_err() || is_terminal(*self.state_rx.borrow()) {
                        return Ok(self.receive_rx.try_recv().ok());
                    }
                }
            }
        }
    }

    pub(crate) async fn close(&self) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(Command::Close {
                id: self.id,
                reply: Some(reply),
            })
            .await
            .map_err(|_| "uTP transport actor stopped".to_string())?;
        response
            .await
            .map_err(|_| "uTP transport actor dropped a close response".to_string())?
    }
}

impl Drop for UtpConnectionHandle {
    fn drop(&mut self) {
        if !self.close_requested.swap(true, Ordering::AcqRel) {
            self.close_notify.notify_one();
        }
    }
}

async fn run_actor(
    mut socket: UtpSocket,
    readiness: std::sync::Arc<tokio::net::UdpSocket>,
    mut commands: mpsc::Receiver<Command>,
    command_tx: mpsc::Sender<Command>,
    incoming_tx: mpsc::Sender<IncomingUtpConnection>,
    close_notify: Arc<Notify>,
    shutdown: CancellationToken,
) {
    let mut connections = HashMap::<u16, ActorConnection>::new();

    loop {
        let timer_delay = socket.next_timer_delay();
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = close_notify.notified() => process_requested_closes(&mut socket, &mut connections),
            command = commands.recv() => {
                let Some(command) = command else { break };
                handle_command(
                    command,
                    &mut socket,
                    &mut connections,
                    &command_tx,
                    &close_notify,
                )
                .await;
                flush_pending_sends(&mut socket, &mut connections);
            }
            ready = readiness.readable() => {
                if let Err(error) = ready {
                    tracing::warn!(%error, "uTP socket readiness failed; stopping transport actor");
                    break;
                }
                let payloads = match socket.poll_recv() {
                    Ok(payloads) => payloads,
                    Err(error) => {
                        // Invalid or out-of-window datagrams are isolated to
                        // this packet; they must not terminate the process-wide socket.
                        tracing::debug!(%error, "Dropped invalid uTP datagram");
                        Vec::new()
                    }
                };
                discover_incoming_connections(
                    &mut socket,
                    &mut connections,
                    &command_tx,
                    &incoming_tx,
                    &close_notify,
                );
                deliver_payloads(&mut socket, payloads, &mut connections);
                flush_pending_sends(&mut socket, &mut connections);
                publish_connection_state(&socket, &mut connections);
            }
            _ = wait_for_timer(timer_delay) => {
                if let Err(error) = socket.process_timers() {
                    tracing::debug!(%error, "uTP timer processing reported a connection error");
                }
                flush_pending_sends(&mut socket, &mut connections);
                publish_connection_state(&socket, &mut connections);
            }
        }
    }

    for (id, mut connection) in connections.drain() {
        let _ = socket.close_connection(id);
        connection.state_tx.send_replace(ConnectionState::Closed);
        while let Some(pending) = connection.pending_sends.pop_front() {
            let _ = pending
                .reply
                .send(Err("uTP transport actor stopped".to_string()));
        }
    }
}

async fn wait_for_timer(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay.max(Duration::from_millis(1))).await,
        None => std::future::pending::<()>().await,
    }
}

async fn handle_command(
    command: Command,
    socket: &mut UtpSocket,
    connections: &mut HashMap<u16, ActorConnection>,
    command_tx: &mpsc::Sender<Command>,
    close_notify: &Arc<Notify>,
) {
    match command {
        Command::Connect { endpoint, reply } => {
            let result = socket.connect(endpoint).map_err(|error| error.to_string());
            match result {
                Ok(id) => {
                    let state = socket
                        .connection_state(id)
                        .unwrap_or(ConnectionState::Closed);
                    let (connection, actor_connection) =
                        new_connection_handle(id, command_tx.clone(), state, close_notify);
                    connections.insert(id, actor_connection);
                    let _ = reply.send(Ok(connection));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        }
        Command::Send { id, bytes, reply } => {
            let Some(connection) = connections.get_mut(&id) else {
                let _ = reply.send(Err(format!("uTP connection {id} is no longer active")));
                return;
            };
            if connection.rejected {
                let _ = reply.send(Err("uTP connection receive queue was exceeded".to_string()));
                return;
            }
            if connection.pending_send_bytes.saturating_add(bytes.len()) > MAX_PENDING_SEND_BYTES {
                let _ = reply.send(Err("uTP per-peer send queue is full".to_string()));
                return;
            }
            connection.pending_send_bytes += bytes.len();
            connection.pending_sends.push_back(PendingSend {
                bytes,
                offset: 0,
                reply,
            });
        }
        Command::Close { id, reply } => {
            let result = socket
                .close_connection(id)
                .map_err(|error| error.to_string());
            if let Some(connection) = connections.get_mut(&id) {
                connection.close_requested.store(true, Ordering::Release);
                connection.state_tx.send_replace(ConnectionState::FinWait);
                fail_pending_sends(connection, "uTP connection is closing");
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            } else if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        }
    }
}

fn new_connection_handle(
    id: u16,
    command_tx: mpsc::Sender<Command>,
    state: ConnectionState,
    close_notify: &Arc<Notify>,
) -> (UtpConnectionHandle, ActorConnection) {
    let (receive_tx, receive_rx) = mpsc::channel(RECEIVE_CAPACITY);
    let (state_tx, state_rx) = watch::channel(state);
    let close_requested = Arc::new(AtomicBool::new(false));
    (
        UtpConnectionHandle {
            id,
            command_tx,
            receive_rx,
            state_rx,
            close_requested: Arc::clone(&close_requested),
            close_notify: Arc::clone(close_notify),
        },
        ActorConnection {
            receive_tx,
            state_tx,
            pending_sends: VecDeque::new(),
            pending_send_bytes: 0,
            rejected: false,
            close_requested,
        },
    )
}

fn discover_incoming_connections(
    socket: &mut UtpSocket,
    connections: &mut HashMap<u16, ActorConnection>,
    command_tx: &mpsc::Sender<Command>,
    incoming_tx: &mpsc::Sender<IncomingUtpConnection>,
    close_notify: &Arc<Notify>,
) {
    for id in socket.connection_ids() {
        if connections.contains_key(&id) {
            continue;
        }
        let Ok(stats) = socket.connection_stats(id) else {
            continue;
        };
        let Some(endpoint) = stats.remote_addr else {
            let _ = socket.close_connection(id);
            continue;
        };
        let (connection, actor_connection) =
            new_connection_handle(id, command_tx.clone(), stats.state, close_notify);
        connections.insert(id, actor_connection);
        if incoming_tx
            .try_send(IncomingUtpConnection {
                connection,
                endpoint,
            })
            .is_err()
        {
            tracing::debug!(%endpoint, "Rejected incoming uTP peer because the admission queue is full");
        }
    }
}

fn process_requested_closes(
    socket: &mut UtpSocket,
    connections: &mut HashMap<u16, ActorConnection>,
) {
    for (id, connection) in connections.iter_mut() {
        if !connection.close_requested.swap(false, Ordering::AcqRel) {
            continue;
        }
        if let Err(error) = socket.close_connection(*id) {
            tracing::debug!(connection_id = *id, %error, "Failed to close dropped uTP peer");
        }
        connection.state_tx.send_replace(ConnectionState::FinWait);
        fail_pending_sends(connection, "uTP connection is closing");
    }
}

fn fail_pending_sends(connection: &mut ActorConnection, reason: &str) {
    connection.pending_send_bytes = 0;
    while let Some(pending) = connection.pending_sends.pop_front() {
        let _ = pending.reply.send(Err(reason.to_string()));
    }
}

fn flush_pending_sends(socket: &mut UtpSocket, connections: &mut HashMap<u16, ActorConnection>) {
    for (id, connection) in connections.iter_mut() {
        loop {
            let Some(pending) = connection.pending_sends.front_mut() else {
                break;
            };
            let remaining = &pending.bytes[pending.offset..];
            match socket.send(*id, remaining) {
                Ok(0) => break,
                Ok(sent) => {
                    pending.offset += sent;
                    connection.pending_send_bytes =
                        connection.pending_send_bytes.saturating_sub(sent);
                    if pending.offset == pending.bytes.len() {
                        let pending = connection.pending_sends.pop_front().unwrap();
                        let _ = pending.reply.send(Ok(()));
                    }
                }
                Err(error) => {
                    let pending = connection.pending_sends.pop_front().unwrap();
                    connection.pending_send_bytes = connection
                        .pending_send_bytes
                        .saturating_sub(pending.bytes.len().saturating_sub(pending.offset));
                    let _ = pending.reply.send(Err(error.to_string()));
                }
            }
        }
    }
}

fn deliver_payloads(
    socket: &mut UtpSocket,
    payloads: Vec<(u16, Vec<u8>)>,
    connections: &mut HashMap<u16, ActorConnection>,
) {
    for (id, payload) in payloads {
        let Some(connection) = connections.get_mut(&id) else {
            continue;
        };
        if connection.rejected {
            continue;
        }
        if connection.receive_tx.try_send(payload).is_err() {
            tracing::debug!(
                connection_id = id,
                "Closing uTP peer with a full receive queue"
            );
            connection.rejected = true;
            let _ = socket.close_connection(id);
        }
    }
}

fn is_terminal(state: ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::Closed
            | ConnectionState::Closing
            | ConnectionState::FinWait
            | ConnectionState::TimeWait
    )
}

fn publish_connection_state(socket: &UtpSocket, connections: &mut HashMap<u16, ActorConnection>) {
    let active = socket.connection_ids();
    connections.retain(|id, connection| {
        if !active.contains(id) {
            connection.state_tx.send_replace(ConnectionState::Closed);
            while let Some(pending) = connection.pending_sends.pop_front() {
                let _ = pending.reply.send(Err("uTP connection closed".to_string()));
            }
            return false;
        }
        let Ok(state) = socket.connection_state(*id) else {
            connection.state_tx.send_replace(ConnectionState::Closed);
            return false;
        };
        connection.state_tx.send_replace(state);
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_connection_requests_close_without_bounded_queue_capacity() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (reply, _response) = oneshot::channel();
        command_tx
            .try_send(Command::Send {
                id: 7,
                bytes: vec![1],
                reply,
            })
            .unwrap();
        let (_receive_tx, receive_rx) = mpsc::channel(1);
        let (_state_tx, state_rx) = watch::channel(ConnectionState::Established);
        let close_requested = Arc::new(AtomicBool::new(false));
        let close_notify = Arc::new(Notify::new());
        let handle = UtpConnectionHandle {
            id: 7,
            command_tx,
            receive_rx,
            state_rx,
            close_requested: Arc::clone(&close_requested),
            close_notify: Arc::clone(&close_notify),
        };

        drop(handle);

        assert!(close_requested.load(Ordering::Acquire));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(Command::Send { id: 7, .. })
        ));
        assert!(matches!(
            command_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        tokio::time::timeout(Duration::from_millis(50), close_notify.notified())
            .await
            .expect("drop close notification must not depend on command-queue capacity");
    }
}
