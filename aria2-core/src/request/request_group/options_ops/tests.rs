use super::runtime_update::apply_runtime_option_value;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

#[test]
fn runtime_option_batch_validates_all_entries_before_mutating() {
    let mut group = RequestGroup::new(
        GroupId::new(1),
        vec!["http://example.com/file".to_string()],
        DownloadOptions::default(),
    );
    let changes = std::collections::HashMap::from([
        ("dir".to_string(), serde_json::json!("new-directory")),
        ("split".to_string(), serde_json::json!("0")),
    ]);

    assert!(group.apply_runtime_options(changes).is_err());
    assert_eq!(group.options().dir, None);
    assert!(group.runtime_options().is_empty());
}

#[test]
fn runtime_split_update_keeps_values_above_u16() {
    let mut options = DownloadOptions::default();
    assert!(
        apply_runtime_option_value(&mut options, "split", &serde_json::json!("1000000")).unwrap()
    );
    assert_eq!(options.split, Some(1_000_000));
}

#[cfg(feature = "sftp")]
#[test]
fn connection_timeouts_are_valid_rpc_task_options() {
    assert_eq!(
        crate::config::is_option_changeable("connect-timeout", false),
        crate::config::ChangeableKind::Immediate
    );
    assert_eq!(
        crate::config::is_option_changeable("connect-timeout", true),
        crate::config::ChangeableKind::Pending
    );
    assert_eq!(
        crate::config::is_option_changeable("sftp-connect-timeout", false),
        crate::config::ChangeableKind::Immediate
    );
    assert_eq!(
        crate::config::is_option_changeable("sftp-read-timeout", true),
        crate::config::ChangeableKind::Pending
    );
    for (name, value) in [
        ("connect-timeout", serde_json::json!(47)),
        ("sftp-connect-timeout", serde_json::json!(41)),
        ("sftp-read-timeout", serde_json::json!(73)),
    ] {
        assert!(
            crate::request::request_group::RequestGroup::validate_option_update(name, &value)
                .unwrap()
        );
    }
    assert!(
        crate::request::request_group::RequestGroup::validate_option_update(
            "sftp-connect-timeout",
            &serde_json::json!(0)
        )
        .is_err()
    );

    let rpc_options = std::collections::HashMap::from([
        ("connect-timeout".to_string(), serde_json::json!(47)),
        ("sftp-connect-timeout".to_string(), serde_json::json!(41)),
        ("sftp-read-timeout".to_string(), serde_json::json!(73)),
        ("sftp-compression".to_string(), serde_json::json!(true)),
        ("sftp-keepalive-interval".to_string(), serde_json::json!(0)),
        (
            "sftp-preferred-ciphers".to_string(),
            serde_json::json!("aes256-gcm@openssh.com,chacha20-poly1305@openssh.com"),
        ),
    ]);
    let parsed = DownloadOptions::try_from_rpc_options(&rpc_options)
        .expect("addUri RPC timeout options should parse");
    assert_eq!(parsed.connect_timeout, Some(47));
    assert_eq!(parsed.sftp_connect_timeout, Some(41));
    assert_eq!(parsed.sftp_read_timeout, Some(73));
    assert!(parsed.sftp_compression);
    assert_eq!(parsed.sftp_keepalive_interval, Some(0));
    assert_eq!(parsed.sftp_preferred_ciphers.as_deref().unwrap().len(), 2);

    let mut options = DownloadOptions::default();
    assert!(
        apply_runtime_option_value(&mut options, "connect-timeout", &serde_json::json!(47))
            .unwrap()
    );
    assert!(
        apply_runtime_option_value(&mut options, "sftp-connect-timeout", &serde_json::json!(41))
            .unwrap()
    );
    assert!(
        apply_runtime_option_value(&mut options, "sftp-read-timeout", &serde_json::json!(73))
            .unwrap()
    );
    assert_eq!(options.sftp_connect_timeout, Some(41));
    assert_eq!(options.sftp_read_timeout, Some(73));
    assert_eq!(options.connect_timeout, Some(47));
}

#[cfg(feature = "sftp")]
#[test]
fn ftp_and_sftp_connection_options_follow_reserved_and_pending_change_policy() {
    for name in [
        "ftp-reuse-connection",
        "ssh-host-key-md",
        "sftp-connect-timeout",
        "sftp-read-timeout",
        "sftp-compression",
        "sftp-keepalive-interval",
        "sftp-preferred-ciphers",
    ] {
        assert_eq!(
            crate::config::is_option_changeable(name, false),
            crate::config::ChangeableKind::Immediate,
            "reserved tasks should accept {name} immediately"
        );
        assert_eq!(
            crate::config::is_option_changeable(name, true),
            crate::config::ChangeableKind::Pending,
            "active tasks should defer {name} until restart"
        );
    }

    let mut options = DownloadOptions::default();
    for (name, value) in [
        ("ftp-reuse-connection", serde_json::json!(false)),
        (
            "ssh-host-key-md",
            serde_json::json!("sha-256=task-fingerprint"),
        ),
        ("sftp-connect-timeout", serde_json::json!(19)),
        ("sftp-read-timeout", serde_json::json!(27)),
        ("sftp-compression", serde_json::json!(false)),
        ("sftp-keepalive-interval", serde_json::json!(15)),
        (
            "sftp-preferred-ciphers",
            serde_json::json!("aes128-ctr,chacha20-poly1305@openssh.com"),
        ),
    ] {
        assert!(apply_runtime_option_value(&mut options, name, &value).unwrap());
    }

    assert!(!options.ftp_reuse_connection);
    assert_eq!(
        options.ssh_host_key_md.as_deref(),
        Some("sha-256=task-fingerprint")
    );
    assert_eq!(options.sftp_connect_timeout, Some(19));
    assert_eq!(options.sftp_read_timeout, Some(27));
    assert!(!options.sftp_compression);
    assert_eq!(options.sftp_keepalive_interval, Some(15));
    assert_eq!(
        options.sftp_preferred_ciphers.as_deref(),
        Some(
            &[
                "aes128-ctr".to_string(),
                "chacha20-poly1305@openssh.com".to_string()
            ][..]
        )
    );
}

#[test]
fn http2_limits_can_be_updated_through_task_rpc_options() {
    let mut options = DownloadOptions::default();
    assert!(
        apply_runtime_option_value(
            &mut options,
            "max-http2-sessions-per-server",
            &serde_json::json!("4")
        )
        .unwrap()
    );
    assert!(
        apply_runtime_option_value(
            &mut options,
            "max-http2-streams-per-session",
            &serde_json::json!("32")
        )
        .unwrap()
    );

    assert_eq!(options.max_http2_sessions_per_server, Some(4));
    assert_eq!(options.max_http2_streams_per_session, Some(32));

    assert!(
        apply_runtime_option_value(
            &mut options,
            "max-http2-sessions-per-server",
            &serde_json::json!("0")
        )
        .is_err()
    );
    assert!(
        apply_runtime_option_value(
            &mut options,
            "max-http2-streams-per-session",
            &serde_json::json!("257")
        )
        .is_err()
    );
}

#[test]
fn adaptive_http_parameters_can_be_updated_through_task_rpc_options() {
    let mut options = DownloadOptions::default();
    for (name, value) in [
        ("http-adaptive-enabled", "false"),
        ("http-adaptive-reduction-percent", "30"),
        ("http-adaptive-backoff-base", "15"),
        ("http-adaptive-backoff-max", "120"),
    ] {
        assert!(apply_runtime_option_value(&mut options, name, &serde_json::json!(value)).unwrap());
    }

    assert!(!options.http_adaptive.enabled);
    assert_eq!(options.http_adaptive.reduction_percent, 30);
    assert_eq!(options.http_adaptive.backoff_base_secs, 15);
    assert_eq!(options.http_adaptive.backoff_max_secs, 120);
    assert!(
        apply_runtime_option_value(
            &mut options,
            "http-adaptive-enabled",
            &serde_json::json!("yes")
        )
        .is_err()
    );
    assert!(
        apply_runtime_option_value(
            &mut options,
            "http-adaptive-reduction-percent",
            &serde_json::json!("0")
        )
        .is_err()
    );
}

#[cfg(feature = "bittorrent")]
#[test]
fn utp_task_options_follow_reserved_and_pending_change_policy() {
    for name in ["enable-utp", "utp-listen-port"] {
        assert_eq!(
            crate::config::is_option_changeable(name, false),
            crate::config::ChangeableKind::Immediate,
            "reserved tasks should accept {name} immediately"
        );
        assert_eq!(
            crate::config::is_option_changeable(name, true),
            crate::config::ChangeableKind::Pending,
            "active tasks should defer {name} until restart"
        );
    }

    let mut options = DownloadOptions::default();
    assert!(
        apply_runtime_option_value(&mut options, "enable-utp", &serde_json::json!(true)).unwrap()
    );
    assert!(
        apply_runtime_option_value(&mut options, "utp-listen-port", &serde_json::json!(6882))
            .unwrap()
    );
    assert!(options.enable_utp);
    assert_eq!(options.utp_listen_port, Some(6882));
}

#[test]
fn string_options_reach_their_typed_fields() {
    let cases = [
        ("bt-peer-blocklist", serde_json::json!("blocklist.txt")),
        ("peer-id-prefix", serde_json::json!("-AZ1234-")),
        ("peer-agent", serde_json::json!("aria2-test")),
        ("dht-listen-addr6", serde_json::json!("[::1]")),
        (
            "dht-entry-point-host",
            serde_json::json!("bootstrap.example"),
        ),
        ("dht-entry-point6", serde_json::json!("[2001:db8::1]:6881")),
        (
            "dht-entry-point-host6",
            serde_json::json!("bootstrap6.example"),
        ),
        ("dht-file-path6", serde_json::json!("dht6.dat")),
        ("dht-listen-addr", serde_json::json!("127.0.0.1")),
    ];

    for (key, value) in cases {
        let mut options = DownloadOptions::default();
        assert!(
            apply_runtime_option_value(&mut options, key, &value).unwrap(),
            "{key}"
        );
    }
}
