#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aria2_protocol::sftp::packet::{
    SSH_FX_EOF, SSH_FX_FAILURE, SSH_FX_NO_SUCH_FILE, SSH_FX_OK, SftpFileAttrs, SftpPacket,
};
use russh::ChannelId;
use russh::keys::PrivateKey;
use russh::keys::ssh_key::{self, rand_core::OsRng};
use russh::server::{self, Auth, Msg, Session};
use sha1::{Digest, Sha1};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const USERNAME: &str = "aria2";
const PASSWORD: &str = "sftp-password";
const FILE_PATH: &str = "/files/fixture.bin";
const FILE_HANDLE: &[u8] = b"aria2-sftp-fixture";

struct FixtureData {
    content: Arc<[u8]>,
    read_delay: Option<Duration>,
}

/// A deterministic, protocol-level SFTP server for command E2E tests.
///
/// The fixture implements only the v3 operations used by a download: INIT,
/// STAT, OPEN, READ, and CLOSE. It deliberately goes through a real SSH
/// handshake and password exchange rather than bypassing the protocol seam.
pub struct MockSftpServer {
    addr: SocketAddr,
    content: Arc<[u8]>,
    sha1_fingerprint: String,
    shutdown: Option<oneshot::Sender<()>>,
    accept_task: JoinHandle<()>,
}

impl MockSftpServer {
    pub async fn start() -> Self {
        Self::start_with_read_delay(None).await
    }

    /// Start a server that delays each SFTP READ response.
    ///
    /// This keeps a real transfer in flight long enough for lifecycle tests to
    /// request pause or removal through the command's shared group.
    pub async fn start_slow() -> Self {
        Self::start_with_read_delay(Some(Duration::from_millis(50))).await
    }

    async fn start_with_read_delay(read_delay: Option<Duration>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock SFTP server should bind");
        let addr = listener
            .local_addr()
            .expect("mock SFTP listener should expose its address");
        let content: Arc<[u8]> = fixture_content().into();

        let mut rng = OsRng;
        let host_key = PrivateKey::random(&mut rng, ssh_key::Algorithm::Ed25519)
            .expect("mock SFTP host key should generate");
        let public_key_bytes = host_key
            .public_key()
            .to_bytes()
            .expect("mock SFTP public key should encode");
        let sha1_fingerprint = format!("sha-1={}", hex::encode(Sha1::digest(public_key_bytes)));

        let config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_key],
            ..Default::default()
        });
        let fixture = Arc::new(FixtureData {
            content: Arc::clone(&content),
            read_delay,
        });
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let accept_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else {
                            break;
                        };
                        let config = Arc::clone(&config);
                        let fixture = Arc::clone(&fixture);
                        tokio::spawn(async move {
                            let handler = MockSftpHandler::new(fixture);
                            if let Ok(session) = server::run_stream(config, stream, handler).await {
                                let _ = session.await;
                            }
                        });
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        Self {
            addr,
            content,
            sha1_fingerprint,
            shutdown: Some(shutdown),
            accept_task,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn username(&self) -> &'static str {
        USERNAME
    }

    pub fn password(&self) -> &'static str {
        PASSWORD
    }

    pub fn file_path(&self) -> &'static str {
        FILE_PATH
    }

    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Uses the `aria2_original` `--ssh-host-key-md` SHA-1 wire format.
    pub fn sha1_fingerprint(&self) -> &str {
        &self.sha1_fingerprint
    }
}

impl Drop for MockSftpServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.accept_task.abort();
    }
}

struct MockSftpHandler {
    fixture: Arc<FixtureData>,
    sftp_channels: HashSet<ChannelId>,
    receive_buffers: HashMap<ChannelId, Vec<u8>>,
}

impl MockSftpHandler {
    fn new(fixture: Arc<FixtureData>) -> Self {
        Self {
            fixture,
            sftp_channels: HashSet::new(),
            receive_buffers: HashMap::new(),
        }
    }

    fn response_for(&self, request: SftpPacket) -> SftpPacket {
        match request {
            SftpPacket::Init { .. } => SftpPacket::Version {
                version: 3,
                extensions: Vec::new(),
            },
            SftpPacket::Stat { request_id, path } | SftpPacket::Lstat { request_id, path } => {
                self.attributes_response(request_id, &path)
            }
            SftpPacket::Open {
                request_id,
                filename,
                ..
            } => {
                if filename == FILE_PATH {
                    SftpPacket::Handle {
                        request_id,
                        handle: FILE_HANDLE.to_vec(),
                    }
                } else {
                    status(request_id, SSH_FX_NO_SUCH_FILE, "No such file")
                }
            }
            SftpPacket::Read {
                request_id,
                handle,
                offset,
                length,
            } => self.read_response(request_id, &handle, offset, length),
            SftpPacket::Close { request_id, .. } => status(request_id, SSH_FX_OK, "OK"),
            request => status(
                request.request_id().unwrap_or_default(),
                SSH_FX_FAILURE,
                "Unsupported fixture operation",
            ),
        }
    }

    fn attributes_response(&self, request_id: u32, path: &str) -> SftpPacket {
        if path != FILE_PATH {
            return status(request_id, SSH_FX_NO_SUCH_FILE, "No such file");
        }

        SftpPacket::Attrs {
            request_id,
            attrs: SftpFileAttrs::full(self.fixture.content.len() as u64, 0, 0, 0o100644, 0, 0),
        }
    }

    fn read_response(
        &self,
        request_id: u32,
        handle: &[u8],
        offset: u64,
        length: u32,
    ) -> SftpPacket {
        if handle != FILE_HANDLE {
            return status(request_id, SSH_FX_FAILURE, "Unknown file handle");
        }

        let Ok(offset) = usize::try_from(offset) else {
            return status(request_id, SSH_FX_EOF, "EOF");
        };
        if offset >= self.fixture.content.len() {
            return status(request_id, SSH_FX_EOF, "EOF");
        }

        let end = offset
            .saturating_add(length as usize)
            .min(self.fixture.content.len());
        SftpPacket::Data {
            request_id,
            data: self.fixture.content[offset..end].to_vec(),
        }
    }
}

impl server::Handler for MockSftpHandler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == USERNAME && password == PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: russh::Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            self.sftp_channels.insert(channel);
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.sftp_channels.contains(&channel) {
            return Ok(());
        }

        let mut requests = Vec::new();
        {
            let buffer = self.receive_buffers.entry(channel).or_default();
            buffer.extend_from_slice(data);
            loop {
                match SftpPacket::decode(buffer) {
                    Ok((request, consumed)) => {
                        buffer.drain(..consumed);
                        requests.push(request);
                    }
                    Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }

        for request in requests {
            let response = self.response_for(request);
            if let Some(delay) = self.fixture.read_delay {
                tokio::time::sleep(delay).await;
            }
            let encoded = response.encode()?;
            session.data(channel, encoded)?;
        }
        Ok(())
    }
}

fn status(request_id: u32, code: u32, message: &str) -> SftpPacket {
    SftpPacket::Status {
        request_id,
        code,
        message: message.to_string(),
        language: String::new(),
    }
}

fn fixture_content() -> Vec<u8> {
    (0..(3 * 64 * 1024 + 137))
        .map(|index| ((index * 31 + 17) % 251) as u8)
        .collect()
}
