use super::*;

#[tokio::test]
async fn set_stat_patch_sends_explicit_zero_values_on_the_wire() {
    let observed_packets = Arc::new(Mutex::new(Vec::new()));
    let mut server = TestSshServer {
        read_started: Arc::new(Mutex::new(None)),
        close_seen: Arc::new(Mutex::new(None)),
        observed_packets: Arc::clone(&observed_packets),
        remote_file_size: 0,
        eof_on_read: false,
        stall_on_stat: false,
        close_status_code: SSH_FX_OK,
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
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
        .expect("connect to SETSTAT fixture");
    let session = SftpSession::open(&mut connection)
        .await
        .expect("initialize SETSTAT fixture session");
    let file_ops = SftpFileOps::new(&session);
    file_ops
        .set_stat_patch(
            "/remote/zero-values",
            &FileAttributePatch {
                size: Some(0),
                owner: Some(FileOwner { uid: 0, gid: 0 }),
                permissions: Some(0),
                timestamps: Some(FileTimestamps { atime: 0, mtime: 0 }),
            },
        )
        .await
        .expect("SETSTAT should accept explicit zero values");

    let packets = observed_packets.lock().await;
    let attrs = packets
        .iter()
        .find_map(|packet| match packet {
            SftpPacket::Setstat { attrs, .. } => Some(attrs),
            _ => None,
        })
        .expect("server should receive SETSTAT");
    assert_eq!(
        attrs.flags,
        SSH_FILEXFER_ATTR_SIZE
            | SSH_FILEXFER_ATTR_UIDGID
            | SSH_FILEXFER_ATTR_PERMISSIONS
            | SSH_FILEXFER_ATTR_ACMODTIME
    );
    assert_eq!(attrs.size, Some(0));
    assert_eq!(attrs.uid, Some(0));
    assert_eq!(attrs.gid, Some(0));
    assert_eq!(attrs.permissions, Some(0));
    assert_eq!(attrs.atime, Some(0));
    assert_eq!(attrs.mtime, Some(0));
    drop(packets);

    server_task.abort();
    let _ = server_task.await;
}
