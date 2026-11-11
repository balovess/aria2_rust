use super::runtime_update::apply_runtime_option_value;
use crate::request::request_group::DownloadOptions;

#[test]
fn runtime_split_update_keeps_values_above_u16() {
    let mut options = DownloadOptions::default();
    assert!(
        apply_runtime_option_value(&mut options, "split", &serde_json::json!("1000000")).unwrap()
    );
    assert_eq!(options.split, Some(1_000_000));
}

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
        ("http-adaptive-recovery-interval", "20"),
        ("http-adaptive-recovery-successes-min", "40"),
        ("http-adaptive-recovery-successes-max", "50"),
        ("http-adaptive-start-interval-ms", "250"),
        ("http-adaptive-backoff-base", "15"),
        ("http-adaptive-backoff-max", "120"),
        ("http-adaptive-capacity-start-interval-ms", "1500"),
        ("http-adaptive-capacity-start-interval-max-ms", "5000"),
        ("http-adaptive-rate-recovery-successes", "80"),
        ("http-adaptive-rate-min-interval-ms", "25"),
        ("http-adaptive-backoff-reset-after", "90"),
    ] {
        assert!(apply_runtime_option_value(&mut options, name, &serde_json::json!(value)).unwrap());
    }

    assert!(!options.http_adaptive.enabled);
    assert_eq!(options.http_adaptive.reduction_percent, 30);
    assert_eq!(options.http_adaptive.recovery_interval_secs, 20);
    assert_eq!(options.http_adaptive.recovery_successes_min, 40);
    assert_eq!(options.http_adaptive.recovery_successes_max, 50);
    assert_eq!(options.http_adaptive.start_interval_ms, 250);
    assert_eq!(options.http_adaptive.backoff_base_secs, 15);
    assert_eq!(options.http_adaptive.backoff_max_secs, 120);
    assert_eq!(options.http_adaptive.capacity_start_interval_ms, 1500);
    assert_eq!(options.http_adaptive.capacity_start_interval_max_ms, 5000);
    assert_eq!(options.http_adaptive.rate_recovery_successes, 80);
    assert_eq!(options.http_adaptive.rate_min_interval_ms, 25);
    assert_eq!(options.http_adaptive.backoff_reset_after_secs, 90);
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
