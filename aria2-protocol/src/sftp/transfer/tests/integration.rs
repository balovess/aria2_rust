use super::*;
use crate::sftp::connection::{HostKeyCheckingMode, SshConnection, SshOptions};
use crate::sftp::file_ops::{FileAttributePatch, FileOwner, FileTimestamps, SftpFileOps};
use crate::sftp::packet::{
    SSH_FILEXFER_ATTR_ACMODTIME, SSH_FILEXFER_ATTR_PERMISSIONS, SSH_FILEXFER_ATTR_SIZE,
    SSH_FILEXFER_ATTR_UIDGID, SSH_FX_FAILURE, SSH_FX_OK, SSH_FXF_TRUNC, SftpFileAttrs, SftpPacket,
};
use crate::sftp::session::{SftpSession, SftpSessionError};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const REMOTE_FILE_SIZE: u64 = 1_000_000;
const REMOTE_HANDLE: &[u8] = b"cancellation-test-handle";

struct TestRng;

impl russh::keys::ssh_key::rand_core::TryRng for TestRng {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut bytes = [0; 4];
        self.try_fill_bytes(&mut bytes)?;
        Ok(u32::from_ne_bytes(bytes))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut bytes = [0; 8];
        self.try_fill_bytes(&mut bytes)?;
        Ok(u64::from_ne_bytes(bytes))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        getrandom::fill(dest).expect("OS random source failed");
        Ok(())
    }
}

impl russh::keys::ssh_key::rand_core::TryCryptoRng for TestRng {}

#[derive(Clone)]
struct TestSshServer {
    read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    observed_packets: Arc<Mutex<Vec<SftpPacket>>>,
    remote_file_size: u64,
    eof_on_read: bool,
    stall_on_stat: bool,
    close_status_code: u32,
}

struct TestSshSession {
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    observed_packets: Arc<Mutex<Vec<SftpPacket>>>,
    remote_file_size: u64,
    eof_on_read: bool,
    stall_on_stat: bool,
    close_status_code: u32,
}

struct SftpTestScenario {
    read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    observed_packets: Arc<Mutex<Vec<SftpPacket>>>,
    remote_file_size: u64,
    eof_on_read: bool,
    stall_on_stat: bool,
    close_status_code: u32,
}

struct StalledSubsystemServer;

struct StalledSubsystemSession {
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

struct StalledChannelOpenServer;

struct StalledChannelOpenSession;

impl server::Server for TestSshServer {
    type Handler = TestSshSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
        TestSshSession {
            channels: Arc::new(Mutex::new(HashMap::new())),
            read_started: Arc::clone(&self.read_started),
            close_seen: Arc::clone(&self.close_seen),
            observed_packets: Arc::clone(&self.observed_packets),
            remote_file_size: self.remote_file_size,
            eof_on_read: self.eof_on_read,
            stall_on_stat: self.stall_on_stat,
            close_status_code: self.close_status_code,
        }
    }
}

impl TestSshSession {
    async fn take_channel(&self, channel_id: ChannelId) -> Channel<Msg> {
        self.channels
            .lock()
            .await
            .remove(&channel_id)
            .expect("SFTP subsystem channel was not registered")
    }
}

impl server::Server for StalledSubsystemServer {
    type Handler = StalledSubsystemSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
        StalledSubsystemSession {
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl server::Handler for StalledSubsystemSession {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        _channel_id: ChannelId,
        _name: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        std::future::pending().await
    }
}

impl server::Server for StalledChannelOpenServer {
    type Handler = StalledChannelOpenSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
        StalledChannelOpenSession
    }
}

impl server::Handler for StalledChannelOpenSession {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        std::future::pending().await
    }
}

impl server::Handler for TestSshSession {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }

        let channel = self.take_channel(channel_id).await;
        session.channel_success(channel_id)?;

        let read_started = Arc::clone(&self.read_started);
        let close_seen = Arc::clone(&self.close_seen);
        let observed_packets = Arc::clone(&self.observed_packets);
        let remote_file_size = self.remote_file_size;
        let eof_on_read = self.eof_on_read;
        let stall_on_stat = self.stall_on_stat;
        let close_status_code = self.close_status_code;
        tokio::spawn(async move {
            let scenario = SftpTestScenario {
                read_started,
                close_seen,
                observed_packets,
                remote_file_size,
                eof_on_read,
                stall_on_stat,
                close_status_code,
            };
            let _ = run_test_sftp(channel.into_stream(), scenario).await;
        });

        Ok(())
    }
}

async fn read_test_packet<S>(stream: &mut S, buffer: &mut Vec<u8>) -> std::io::Result<SftpPacket>
where
    S: AsyncRead + Unpin,
{
    loop {
        match SftpPacket::decode(buffer) {
            Ok((packet, consumed)) => {
                buffer.drain(..consumed);
                return Ok(packet);
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(error) => return Err(error),
        }

        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "SFTP test channel closed",
            ));
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
}

async fn write_test_packet<S>(stream: &mut S, packet: &SftpPacket) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream.write_all(&packet.encode()?).await?;
    stream.flush().await
}

async fn run_test_sftp<S>(mut stream: S, scenario: SftpTestScenario) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let SftpTestScenario {
        read_started,
        close_seen,
        observed_packets,
        remote_file_size,
        eof_on_read,
        stall_on_stat,
        close_status_code,
    } = scenario;
    let mut buffer = Vec::new();
    match read_test_packet(&mut stream, &mut buffer).await? {
        SftpPacket::Init { version } => {
            assert_eq!(version, 3);
        }
        packet => panic!("expected SFTP INIT, got {packet:?}"),
    }
    write_test_packet(
        &mut stream,
        &SftpPacket::Version {
            version: 3,
            extensions: Vec::new(),
        },
    )
    .await?;

    loop {
        let packet = read_test_packet(&mut stream, &mut buffer).await?;
        observed_packets.lock().await.push(packet.clone());
        match packet {
            SftpPacket::Stat { .. } if stall_on_stat => {
                if let Some(sender) = read_started.lock().await.take() {
                    let _ = sender.send(());
                }
                std::future::pending::<()>().await;
                return Ok(());
            }
            SftpPacket::Stat { request_id, .. } => {
                write_test_packet(
                    &mut stream,
                    &SftpPacket::Attrs {
                        request_id,
                        attrs: SftpFileAttrs::full(remote_file_size, 0, 0, 0o100644, 0, 0),
                    },
                )
                .await?;
            }
            SftpPacket::Lstat { request_id, .. } => {
                write_test_packet(
                    &mut stream,
                    &SftpPacket::Attrs {
                        request_id,
                        attrs: SftpFileAttrs::full(remote_file_size, 0, 0, 0o100644, 0, 0),
                    },
                )
                .await?;
            }
            SftpPacket::Open { request_id, .. } => {
                write_test_packet(
                    &mut stream,
                    &SftpPacket::Handle {
                        request_id,
                        handle: REMOTE_HANDLE.to_vec(),
                    },
                )
                .await?;
            }
            SftpPacket::Read { request_id, .. } => {
                if eof_on_read {
                    write_test_packet(
                        &mut stream,
                        &SftpPacket::Status {
                            request_id,
                            code: crate::sftp::packet::SSH_FX_EOF,
                            message: "eof".to_string(),
                            language: "en".to_string(),
                        },
                    )
                    .await?;
                } else {
                    write_test_packet(
                        &mut stream,
                        &SftpPacket::Data {
                            request_id,
                            data: b"partial-data".to_vec(),
                        },
                    )
                    .await?;
                }

                if let Some(sender) = read_started.lock().await.take() {
                    let _ = sender.send(());
                }
            }
            SftpPacket::Write { request_id, .. } => {
                write_test_packet(
                    &mut stream,
                    &SftpPacket::Status {
                        request_id,
                        code: SSH_FX_OK,
                        message: "ok".to_string(),
                        language: "en".to_string(),
                    },
                )
                .await?;
            }
            SftpPacket::Setstat { request_id, .. } => {
                write_test_packet(
                    &mut stream,
                    &SftpPacket::Status {
                        request_id,
                        code: SSH_FX_OK,
                        message: "ok".to_string(),
                        language: "en".to_string(),
                    },
                )
                .await?;
            }
            SftpPacket::Close { request_id, .. } => {
                write_test_packet(
                    &mut stream,
                    &SftpPacket::Status {
                        request_id,
                        code: close_status_code,
                        message: if close_status_code == SSH_FX_OK {
                            "ok".to_string()
                        } else {
                            "close failed".to_string()
                        },
                        language: "en".to_string(),
                    },
                )
                .await?;

                if let Some(sender) = close_seen.lock().await.take() {
                    let _ = sender.send(());
                }
                return Ok(());
            }
            packet => panic!("unexpected SFTP packet: {packet:?}"),
        }
    }
}

async fn connect_test_sftp_server(
    remote_file_size: u64,
    close_status_code: u32,
) -> (
    SshConnection,
    SftpSession,
    Arc<Mutex<Vec<SftpPacket>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let observed_packets = Arc::new(Mutex::new(Vec::new()));
    let mut server = TestSshServer {
        read_started: Arc::new(Mutex::new(None)),
        close_seen: Arc::new(Mutex::new(None)),
        observed_packets: Arc::clone(&observed_packets),
        remote_file_size,
        eof_on_read: false,
        stall_on_stat: false,
        close_status_code,
    };
    let mut rng = TestRng;
    let mut config = server::Config::default();
    config.keys.push(
        russh::keys::PrivateKey::random(&mut rng, russh::keys::ssh_key::Algorithm::Ed25519)
            .unwrap(),
    );
    config.auth_rejection_time_initial = Some(Duration::from_millis(1));
    let config = Arc::new(config);
    let server_task = tokio::spawn(async move {
        let (socket, peer_addr) = listener.accept().await.unwrap();
        let handler = server.new_client(Some(peer_addr));
        let running = server::run_stream(config, socket, handler).await.unwrap();
        let _ = running.await;
    });

    let options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let mut connection = SshConnection::connect(options)
        .await
        .expect("connect to SFTP fixture");
    let session = SftpSession::open(&mut connection)
        .await
        .expect("initialize SFTP fixture session");
    (connection, session, observed_packets, server_task)
}

mod cases;
