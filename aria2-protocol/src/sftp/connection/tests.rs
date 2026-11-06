use std::path::PathBuf;
use std::time::Duration;

use super::*;

#[test]
fn test_ssh_options_defaults() {
    let opts = SshOptions::default();
    assert_eq!(opts.port, 22);
    assert_eq!(opts.username, "");
    assert!(opts.password.is_none());
    assert!(opts.private_key_path.is_none());
    assert_eq!(opts.connect_timeout, Duration::from_secs(15));
    assert_eq!(opts.read_timeout, Duration::from_secs(30));
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
fn test_sftp_packet_constants_accessible() {
    // Verify that packet constants are accessible from this module
    assert_eq!(SSH_FXP_INIT, 1);
    assert_eq!(SSH_FXP_VERSION, 2);
}
