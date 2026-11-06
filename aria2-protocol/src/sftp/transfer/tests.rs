use super::*;

#[cfg(feature = "sftp")]
mod integration {
    use super::*;
    use crate::sftp::connection::{HostKeyCheckingMode, SshConnection, SshOptions};
    use crate::sftp::packet::{SSH_FX_OK, SftpFileAttrs, SftpPacket};
    use crate::sftp::session::SftpSession;
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
            getrandom::getrandom(dest).expect("OS random source failed");
            Ok(())
        }
    }

    impl russh::keys::ssh_key::rand_core::TryCryptoRng for TestRng {}

    #[derive(Clone)]
    struct TestSshServer {
        read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    struct TestSshSession {
        channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
        read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl server::Server for TestSshServer {
        type Handler = TestSshSession;

        fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
            TestSshSession {
                channels: Arc::new(Mutex::new(HashMap::new())),
                read_started: Arc::clone(&self.read_started),
                close_seen: Arc::clone(&self.close_seen),
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

    impl server::Handler for TestSshSession {
        type Error = anyhow::Error;

        async fn auth_password(
            &mut self,
            _user: &str,
            _password: &str,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            _session: &mut Session,
        ) -> Result<bool, Self::Error> {
            self.channels.lock().await.insert(channel.id(), channel);
            Ok(true)
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
            tokio::spawn(async move {
                let _ = run_test_sftp(channel.into_stream(), read_started, close_seen).await;
            });

            Ok(())
        }
    }

    async fn read_test_packet<S>(
        stream: &mut S,
        buffer: &mut Vec<u8>,
    ) -> std::io::Result<SftpPacket>
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

    async fn run_test_sftp<S>(
        mut stream: S,
        read_started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        close_seen: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    ) -> std::io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
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
            match packet {
                SftpPacket::Lstat { request_id, .. } => {
                    write_test_packet(
                        &mut stream,
                        &SftpPacket::Attrs {
                            request_id,
                            attrs: SftpFileAttrs::full(REMOTE_FILE_SIZE, 0, 0, 0o100644, 0, 0),
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
                    write_test_packet(
                        &mut stream,
                        &SftpPacket::Data {
                            request_id,
                            data: b"partial-data".to_vec(),
                        },
                    )
                    .await?;

                    if let Some(sender) = read_started.lock().await.take() {
                        let _ = sender.send(());
                    }
                }
                SftpPacket::Close { request_id, .. } => {
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

                    if let Some(sender) = close_seen.lock().await.take() {
                        let _ = sender.send(());
                    }
                    return Ok(());
                }
                packet => panic!("unexpected SFTP packet: {packet:?}"),
            }
        }
    }

    #[tokio::test]
    async fn cancellation_closes_an_active_sftp_download() {
        let (read_started_sender, read_started_receiver) = oneshot::channel();
        let (close_seen_sender, close_seen_receiver) = oneshot::channel();
        let read_started = Arc::new(Mutex::new(Some(read_started_sender)));
        let close_seen = Arc::new(Mutex::new(Some(close_seen_sender)));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut server = TestSshServer {
            read_started: Arc::clone(&read_started),
            close_seen: Arc::clone(&close_seen),
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

        let cancellation = CancellationToken::new();
        let client_cancellation = cancellation.clone();
        let local_path = std::path::PathBuf::from("target")
            .join(format!("sftp-cancellation-{}.bin", std::process::id()));
        tokio::fs::create_dir_all("target")
            .await
            .expect("failed to create the Cargo test output directory");
        let options = SshOptions::new("127.0.0.1", "test-user")
            .with_port(port)
            .with_password("test-password")
            .with_host_key_mode(HostKeyCheckingMode::Disable)
            .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));

        let client_local_path = local_path.clone();
        let client_task = tokio::spawn(async move {
            let mut connection = SshConnection::connect(options)
                .await
                .expect("failed to connect to in-process SFTP server");
            let session = SftpSession::open(&mut connection)
                .await
                .expect("failed to initialize the in-process SFTP session");
            SftpTransfer::new(&session)
                .download_with_cancellation(
                    "/remote/cancellation.bin",
                    &client_local_path,
                    &TransferOptions::default(),
                    &client_cancellation,
                )
                .await
        });

        timeout(Duration::from_secs(5), read_started_receiver)
            .await
            .expect("SFTP server did not reach the active READ")
            .expect("SFTP server readiness signal was dropped");
        cancellation.cancel();

        let result = timeout(Duration::from_secs(5), client_task)
            .await
            .expect("cancellable SFTP download did not finish")
            .expect("cancellable SFTP client task panicked");
        assert!(matches!(result, Err(TransferError::Cancelled)));

        timeout(Duration::from_secs(5), close_seen_receiver)
            .await
            .expect("SFTP client did not close the remote handle")
            .expect("SFTP close signal was dropped");

        let _ = tokio::fs::remove_file(&local_path).await;
        server_task.abort();
        let _ = server_task.await;
    }
}

#[test]
fn test_transfer_options_defaults() {
    let opts = TransferOptions::default();
    assert_eq!(opts.buffer_size, TRANSFER_BUF_SIZE);
    assert_eq!(opts.resume_offset, 0);
    assert!(!opts.preserve_permissions);
    assert!(!opts.preserve_time);
    assert!(opts.progress_callback.is_none());
}

#[test]
fn test_transfer_options_builder() {
    let opts = TransferOptions::default()
        .with_resume(4096)
        .with_buffer_size(128 * 1024)
        .preserve_metadata();

    assert_eq!(opts.resume_offset, 4096);
    assert_eq!(opts.buffer_size, 131072); // 128KB
    assert!(opts.preserve_permissions);
    assert!(opts.preserve_time);
}

#[test]
fn test_transfer_options_buffer_clamp() {
    let small = TransferOptions::default().with_buffer_size(512); // Below min
    assert_eq!(small.buffer_size, MIN_BUFFER_SIZE);

    let large = TransferOptions::default().with_buffer_size(2048 * 1024); // Above max
    assert_eq!(large.buffer_size, MAX_BUFFER_SIZE);

    let exact = TransferOptions::default().with_buffer_size(32768); // Valid
    assert_eq!(exact.buffer_size, 32768);
}

#[test]
fn test_transfer_options_progress_callback() {
    let callback_invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag_clone = callback_invoked.clone();

    let opts = TransferOptions::default().with_progress_callback(move |_, _, _| {
        flag_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    if let Some(ref cb) = opts.progress_callback {
        cb(1000, 5000, 1024.0);
    }
    assert!(callback_invoked.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn test_progress_percent_zero_total() {
    let prog = TransferProgress {
        bytes_transferred: 0,
        total_bytes: 0,
        speed_bytes_per_sec: 0.0,
        elapsed_secs: 0.0,
    };
    assert!((prog.percent() - 100.0).abs() < 0.001);
    assert!(prog.is_complete());
    assert_eq!(prog.remaining(), 0);
}

#[test]
fn test_progress_partial() {
    let prog = TransferProgress {
        bytes_transferred: 500,
        total_bytes: 1000,
        speed_bytes_per_sec: 250.0,
        elapsed_secs: 2.0,
    };
    assert!((prog.percent() - 50.0).abs() < 0.01);
    assert!(!prog.is_complete());
    assert_eq!(prog.remaining(), 500);

    let eta = prog.eta_secs();
    assert!(eta.is_some());
    assert!((eta.unwrap() - 2.0).abs() < 0.01);
}

#[test]
fn test_progress_complete() {
    let prog = TransferProgress {
        bytes_transferred: 1000,
        total_bytes: 1000,
        speed_bytes_per_sec: 500.0,
        elapsed_secs: 2.0,
    };
    assert!((prog.percent() - 100.0).abs() < 0.01);
    assert!(prog.is_complete());
    assert_eq!(prog.remaining(), 0);
    assert!(prog.eta_secs().is_none()); // No remaining = no ETA
}

#[test]
fn test_progress_display_format() {
    let prog = TransferProgress {
        bytes_transferred: 524288,     // 512KB
        total_bytes: 1048576,          // 1MB
        speed_bytes_per_sec: 262144.0, // 256KB/s
        elapsed_secs: 2.0,
    };
    let display = format!("{}", prog);
    assert!(display.contains("50.0%")); // ~50%
    assert!(display.contains("524288"));
    assert!(display.contains("256")); // KB/s
    assert!(display.contains("ETA"));
}

#[test]
fn test_progress_eta_with_zero_speed() {
    let prog = TransferProgress {
        bytes_transferred: 100,
        total_bytes: 10000,
        speed_bytes_per_sec: 0.0,
        elapsed_secs: 10.0,
    };
    assert!(prog.eta_secs().is_none()); // Cannot calculate ETA with zero speed
}

#[test]
fn test_constants() {
    assert_eq!(TRANSFER_BUF_SIZE, 65536); // 64KB
    assert_eq!(PROGRESS_REPORT_INTERVAL, 262144); // 256KB
    assert_eq!(MIN_BUFFER_SIZE, 1024); // 1KB
    assert_eq!(MAX_BUFFER_SIZE, 1048576); // 1MB
}
