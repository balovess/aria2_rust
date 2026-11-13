use std::path::PathBuf;
use std::time::Duration;

use super::*;

struct TestRng;

impl keys::ssh_key::rand_core::TryRng for TestRng {
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

impl keys::ssh_key::rand_core::TryCryptoRng for TestRng {}

#[test]
fn test_ssh_options_defaults() {
    let opts = SshOptions::default();
    assert_eq!(opts.port, 22);
    assert_eq!(opts.username, "");
    assert!(opts.password.is_none());
    assert!(opts.private_key_path.is_none());
    assert_eq!(
        opts.connect_timeout,
        Duration::from_secs(crate::DEFAULT_SFTP_CONNECT_TIMEOUT_SECS)
    );
    assert_eq!(
        opts.read_timeout,
        Duration::from_secs(crate::DEFAULT_SFTP_READ_TIMEOUT_SECS)
    );
    assert!(matches!(opts.host_key_mode, HostKeyCheckingMode::Strict));
}

#[test]
fn test_ssh_options_new() {
    let opts = SshOptions::new("example.com", "user");
    assert_eq!(opts.host, "example.com");
    assert_eq!(opts.username, "user");
    assert_eq!(opts.port, 22);
    assert_eq!(opts.target(), "user@example.com:22");
}

#[test]
fn test_ssh_options_builder_pattern() {
    let opts = SshOptions::new("192.168.1.100", "admin")
        .with_port(2222)
        .with_password("secret123")
        .with_host_key_mode(HostKeyCheckingMode::AcceptNew)
        .with_timeouts(Duration::from_secs(10), Duration::from_secs(60))
        .with_compression(true);

    assert_eq!(opts.port, 2222);
    assert_eq!(opts.password.as_deref(), Some("secret123"));
    assert!(matches!(opts.host_key_mode, HostKeyCheckingMode::AcceptNew));
    assert_eq!(opts.connect_timeout, Duration::from_secs(10));
    assert_eq!(opts.read_timeout, Duration::from_secs(60));
    assert!(opts.compression);
}

#[test]
fn ssh_transport_options_are_applied_to_russh_config() {
    let options = SshOptions::new("example.com", "user")
        .with_keepalive_interval(None)
        .with_compression(true)
        .with_preferred_ciphers(["chacha20-poly1305@openssh.com"]);
    let config = build_client_config(&options).unwrap();

    assert_eq!(config.keepalive_interval, None);
    assert_eq!(
        config.preferred.cipher[0].as_ref(),
        "chacha20-poly1305@openssh.com"
    );
    assert_eq!(config.preferred.compression[0].as_ref(), "zlib@openssh.com");

    let compression_disabled = build_client_config(&SshOptions::default()).unwrap();
    assert_eq!(compression_disabled.preferred.compression.len(), 1);
    assert_eq!(
        compression_disabled.preferred.compression[0].as_ref(),
        "none"
    );
}

#[test]
fn ssh_transport_rejects_unsupported_preferred_cipher() {
    let options =
        SshOptions::new("example.com", "user").with_preferred_ciphers(["not-a-real-cipher"]);

    assert!(matches!(
        build_client_config(&options),
        Err(SshError::Config { message }) if message.contains("Unsupported SSH cipher")
    ));
}

#[test]
fn test_ssh_options_with_private_key() {
    let opts = SshOptions::new("server.example.com", "deploy")
        .with_private_key("/home/deploy/.ssh/id_ed25519")
        .with_passphrase("my_secret_phrase");

    assert_eq!(
        opts.private_key_path.as_deref(),
        Some("/home/deploy/.ssh/id_ed25519")
    );
    assert_eq!(
        opts.private_key_passphrase.as_deref(),
        Some("my_secret_phrase")
    );
}

#[test]
fn test_host_key_modes() {
    let strict = HostKeyCheckingMode::Strict;
    let accept_new = HostKeyCheckingMode::AcceptNew;
    let disable = HostKeyCheckingMode::Disable;

    assert_eq!(strict.to_string(), "strict");
    assert_eq!(accept_new.to_string(), "accept-new");
    assert_eq!(disable.to_string(), "disable");

    assert_ne!(strict, accept_new);
    assert_eq!(HostKeyCheckingMode::default(), HostKeyCheckingMode::Strict);
}

#[test]
fn test_has_auth_credentials() {
    let opts_pwd = SshOptions::new("h", "u").with_password("p");
    assert!(opts_pwd.has_auth_credentials());

    let opts_key = SshOptions::new("h", "u").with_private_key("/path/to/key");
    assert!(opts_key.has_auth_credentials());

    let opts_none = SshOptions::new("h", "u");
    assert!(!opts_none.has_auth_credentials());
}

#[test]
fn test_target_formatting() {
    assert_eq!(
        SshOptions::new("localhost", "root").target(),
        "root@localhost:22"
    );
    assert_eq!(
        SshOptions::new("10.0.0.1", "admin")
            .with_port(2222)
            .target(),
        "admin@10.0.0.1:2222"
    );
}

#[test]
fn test_resolve_key_path_explicit() {
    let opts = SshOptions::new("h", "u").with_private_key("/custom/key");
    assert_eq!(opts.resolve_key_path(), Some(PathBuf::from("/custom/key")));
}

#[test]
fn test_ssh_error_retryable_classification() {
    let timeout_err = SshError::ConnectTimeout {
        host: "h".into(),
        port: 22,
        timeout_secs: 15,
    };
    assert!(timeout_err.is_retryable());
    assert!(!timeout_err.is_auth_failure());

    let auth_err = SshError::AuthFailed {
        method: "password".into(),
        message: "bad pass".into(),
    };
    assert!(!auth_err.is_retryable());
    assert!(auth_err.is_auth_failure());
}

#[test]
fn test_ssh_error_user_messages() {
    let err = SshError::ConnectTimeout {
        host: "example.com".into(),
        port: 22,
        timeout_secs: 30,
    };
    assert!(err.user_message().contains("timed out"));

    let err = SshError::AuthFailed {
        method: "publickey".into(),
        message: "key rejected".into(),
    };
    assert!(err.user_message().contains("Authentication failed"));
}

#[test]
fn rejected_russh_authentication_is_not_treated_as_success() {
    let error = SshConnection::require_successful_authentication(
        client::AuthResult::Failure {
            remaining_methods: russh::MethodSet::empty(),
            partial_success: false,
        },
        "password",
    )
    .expect_err("a rejected SSH password must fail authentication");

    assert!(matches!(
        error,
        SshError::AuthFailed { method, .. } if method == "password"
    ));
}

#[tokio::test]
async fn connect_with_stream_times_out_when_the_ssh_peer_stalls() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind stalled SSH peer");
    let address = listener.local_addr().expect("read listener address");
    let peer = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.expect("accept SSH client");
        std::future::pending::<()>().await;
    });
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to stalled SSH peer");
    let options = SshOptions::new("127.0.0.1", "user")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_millis(50), Duration::from_secs(1));

    let result = SshConnection::connect_with_stream(options, stream).await;
    peer.abort();

    assert!(matches!(
        result,
        Err(SshError::ConnectTimeout {
            host,
            port: 22,
            timeout_secs: 0,
        }) if host == "127.0.0.1"
    ));
}

struct StalledAuthenticationServer {
    authentication_started: Option<tokio::sync::oneshot::Sender<()>>,
}

struct StalledAuthenticationSession {
    authentication_started: Option<tokio::sync::oneshot::Sender<()>>,
}

impl russh::server::Server for StalledAuthenticationServer {
    type Handler = StalledAuthenticationSession;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
        StalledAuthenticationSession {
            authentication_started: self.authentication_started.take(),
        }
    }
}

impl russh::server::Handler for StalledAuthenticationSession {
    type Error = anyhow::Error;

    async fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        if let Some(started) = self.authentication_started.take() {
            let _ = started.send(());
        }
        std::future::pending().await
    }
}

#[tokio::test]
async fn connect_with_stream_times_out_when_authentication_stalls() {
    use russh::server::{self, Server as _};
    use std::sync::Arc;
    use tokio::sync::oneshot;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind stalled authentication peer");
    let address = listener.local_addr().expect("read listener address");
    let (authentication_started, authentication_started_rx) = oneshot::channel();
    let mut server = StalledAuthenticationServer {
        authentication_started: Some(authentication_started),
    };
    let mut rng = TestRng;
    let mut config = server::Config::default();
    config.keys.push(
        keys::PrivateKey::random(&mut rng, keys::ssh_key::Algorithm::Ed25519)
            .expect("generate SSH server key"),
    );
    config.auth_rejection_time_initial = Some(Duration::from_millis(1));
    let config = Arc::new(config);
    let server_task = tokio::spawn(async move {
        let (socket, peer_addr) = listener.accept().await.expect("accept SSH client");
        let handler = server.new_client(Some(peer_addr));
        let running = server::run_stream(config, socket, handler)
            .await
            .expect("start stalled authentication server");
        let _ = running.await;
    });

    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to stalled authentication peer");
    let connect_timeout = Duration::from_millis(300);
    let options = SshOptions::new("127.0.0.1", "user")
        .with_port(address.port())
        .with_password("password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(connect_timeout, Duration::from_secs(1));
    let started_at = std::time::Instant::now();
    let client_task = tokio::spawn(SshConnection::connect_with_stream(options, stream));

    tokio::time::timeout(Duration::from_secs(2), authentication_started_rx)
        .await
        .expect("client did not reach password authentication")
        .expect("authentication server stopped before receiving credentials");
    let result = tokio::time::timeout(Duration::from_secs(2), client_task)
        .await
        .expect("connection timeout did not finish")
        .expect("client task panicked");
    server_task.abort();
    let _ = server_task.await;

    assert!(matches!(
        result,
        Err(SshError::ConnectTimeout {
            host,
            port,
            timeout_secs: 0,
        }) if host == "127.0.0.1" && port == address.port()
    ));
    assert!(started_at.elapsed() < Duration::from_secs(2));
}

#[test]
fn test_host_key_fingerprint_formats() {
    let key = keys::parse_public_key_base64(
        "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ",
    )
    .expect("fixture key must parse");
    let key_bytes = key.to_bytes().expect("fixture key must encode");

    let md5_digest = format!("md5={}", hex::encode(Md5::digest(&key_bytes)));
    let sha1_digest = format!("sha-1={}", hex::encode(Sha1::digest(&key_bytes)));
    let sha256_digest = key.fingerprint(HashAlg::Sha256).to_string();

    assert!(matches_fingerprint(&key, &md5_digest));
    assert!(matches_fingerprint(&key, &sha1_digest));
    assert!(matches_fingerprint(
        &key,
        &format!("sha-256={sha256_digest}")
    ));
    assert!(!matches_fingerprint(&key, "sha-1=00"));
    assert!(!matches_fingerprint(&key, "unknown=00"));
}

#[test]
fn strict_host_key_mode_rejects_unknown_keys_and_accepts_recorded_keys() {
    let directory = tempfile::tempdir().unwrap();
    let known_hosts = directory.path().join("known_hosts");
    let key = keys::parse_public_key_base64(
        "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ",
    )
    .unwrap();
    let options = SshOptions::new("sftp.example", "user");

    assert!(matches!(
        verify_server_key(&options, &key, &known_hosts),
        Err(SshError::Config { message }) if message.contains("not present")
    ));

    russh::keys::known_hosts::learn_known_hosts_path("sftp.example", 22, &key, &known_hosts)
        .unwrap();
    assert!(verify_server_key(&options, &key, &known_hosts).unwrap());
}

#[test]
fn accept_new_host_key_persists_first_key_and_rejects_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let known_hosts = directory.path().join("nested").join("known_hosts");
    let first_key = keys::parse_public_key_base64(
        "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ",
    )
    .unwrap();
    let replacement_key = keys::parse_public_key_base64(
        "AAAAC3NzaC1lZDI1NTE5AAAAIK2pr+Bfus1hvPof4Wb/plZCzSbqadHH0u+/f4ODtKnE",
    )
    .unwrap();
    let options =
        SshOptions::new("sftp.example", "user").with_host_key_mode(HostKeyCheckingMode::AcceptNew);

    assert!(verify_server_key(&options, &first_key, &known_hosts).unwrap());
    assert!(verify_server_key(&options, &first_key, &known_hosts).unwrap());
    assert!(matches!(
        verify_server_key(&options, &replacement_key, &known_hosts),
        Err(SshError::Config { message }) if message.contains("host key changed")
    ));
}

#[test]
fn test_sftp_packet_constants_accessible() {
    // Verify that packet constants are accessible from this module
    assert_eq!(SSH_FXP_INIT, 1);
    assert_eq!(SSH_FXP_VERSION, 2);
}
