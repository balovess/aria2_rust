use super::{
    ChangeableKind, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
    is_global_option_changeable, is_initial_option, is_option_available, is_option_changeable,
    project_initial_options,
};
#[test]
fn policy_matches_original_wire_names() {
    assert!(is_global_option_changeable("dir"));
    assert!(is_global_option_changeable("save-session"));
    assert!(is_global_option_changeable("min-http-range-size"));
    assert!(is_global_option_changeable("max-http2-sessions-per-server"));
    assert!(is_global_option_changeable("max-http2-streams-per-session"));
    assert!(is_global_option_changeable("http-adaptive-enabled"));
    assert!(is_global_option_changeable(
        "http-adaptive-reduction-percent"
    ));
    assert!(is_global_option_changeable("http-adaptive-backoff-max"));
    #[cfg(feature = "bittorrent")]
    assert!(is_global_option_changeable(
        "bt-incoming-peer-handshake-timeout"
    ));
    #[cfg(feature = "bittorrent")]
    assert!(is_global_option_changeable("bt-tracker-source-concurrency"));
    #[cfg(feature = "bittorrent")]
    for name in [
        "dht-max-peer-info-hashes",
        "dht-max-peers-per-info-hash",
        "dht-max-items",
    ] {
        assert!(is_global_option_changeable(name));
    }
    #[cfg(feature = "bittorrent")]
    for name in [
        "bt-utp-max-connections",
        "bt-utp-connect-timeout",
        "bt-utp-idle-timeout",
        "bt-utp-keepalive-interval",
    ] {
        assert!(is_global_option_changeable(name));
    }
    for name in [
        "max-http2-sessions-per-server",
        "max-http2-streams-per-session",
        "http-adaptive-enabled",
        "http-adaptive-reduction-percent",
        "http-adaptive-backoff-max",
    ] {
        assert_eq!(
            is_option_changeable(name, false),
            ChangeableKind::Immediate,
            "reserved task RPC should accept {name} immediately"
        );
        assert_eq!(
            is_option_changeable(name, true),
            ChangeableKind::Pending,
            "active task RPC should store {name} for restart"
        );
    }
    #[cfg(feature = "bittorrent")]
    assert!(is_global_option_changeable("bt-force-encryption"));
    #[cfg(not(feature = "bittorrent"))]
    assert!(!is_global_option_changeable("bt-force-encryption"));
    assert!(!is_global_option_changeable("no-conf"));
    assert!(!is_global_option_changeable("show-files"));
}

#[test]
fn compatibility_aliases_use_the_canonical_runtime_policy() {
    let aliases = [
        ("max-retries", "max-tries"),
        ("enable-lpd", "bt-enable-lpd"),
        ("dht-message-path", "dht-file-path"),
        ("server-stat-file", "server-stat-of"),
        ("max-downloads", "max-concurrent-downloads"),
    ];

    for (alias, canonical) in aliases {
        assert_eq!(
            is_option_available(alias),
            is_option_available(canonical),
            "runtime availability for {alias} must match {canonical}"
        );
        assert_eq!(
            is_initial_option(alias),
            is_initial_option(canonical),
            "initial-option policy for {alias} must match {canonical}"
        );
        assert_eq!(
            is_global_option_changeable(alias),
            is_global_option_changeable(canonical),
            "global runtime policy for {alias} must match {canonical}"
        );
        for active in [false, true] {
            assert_eq!(
                is_option_changeable(alias, active),
                is_option_changeable(canonical, active),
                "task runtime policy for {alias} must match {canonical}"
            );
        }
    }

    assert!(is_global_option_changeable("max-downloads"));

    #[cfg(feature = "bittorrent")]
    {
        assert!(is_initial_option("enable-lpd"));
        assert_eq!(
            is_option_changeable("enable-lpd", false),
            ChangeableKind::Immediate
        );
        assert_eq!(
            is_option_changeable("enable-lpd", true),
            ChangeableKind::Pending
        );
    }

    #[cfg(not(feature = "bittorrent"))]
    {
        assert!(!is_initial_option("enable-lpd"));
        assert_eq!(
            is_option_changeable("enable-lpd", false),
            ChangeableKind::NotChangeable
        );
    }
}

#[test]
fn initial_option_projection_keeps_request_options_only() {
    let projected = project_initial_options(std::collections::HashMap::from([
        ("dir".to_string(), serde_json::json!("/downloads")),
        ("rpc-secret".to_string(), serde_json::json!("secret")),
        ("enable-rpc".to_string(), serde_json::json!("true")),
        (
            "aria2-rust-metadata-uri".to_string(),
            serde_json::json!("https://example.test/metadata.torrent"),
        ),
    ]));

    assert_eq!(projected.get("dir"), Some(&serde_json::json!("/downloads")));
    assert!(!projected.contains_key("rpc-secret"));
    assert!(!projected.contains_key("enable-rpc"));
    assert!(!projected.contains_key("aria2-rust-metadata-uri"));
    assert!(is_initial_option("dir"));
    assert!(!is_initial_option("rpc-secret"));
}

#[test]
fn task_policy_matches_runtime_changeability_axes() {
    assert_eq!(RUNTIME_CHANGEABLE_OPTIONS.len(), 7);
    #[cfg(feature = "bittorrent")]
    assert_eq!(RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS.len(), 131);
    #[cfg(not(feature = "bittorrent"))]
    assert_eq!(RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS.len(), 130);
    assert_eq!(
        is_option_changeable("max-download-limit", true),
        ChangeableKind::Immediate
    );
    assert_eq!(is_option_changeable("dir", true), ChangeableKind::Pending);
    assert_eq!(
        is_option_changeable("min-http-range-size", true),
        ChangeableKind::Pending
    );
    assert_eq!(
        is_option_changeable("min-http-range-size", false),
        ChangeableKind::Immediate
    );
    for option in [
        "sftp-connect-timeout",
        "sftp-read-timeout",
        "sftp-compression",
        "sftp-keepalive-interval",
        "sftp-preferred-ciphers",
    ] {
        assert_eq!(
            is_option_changeable(option, true),
            ChangeableKind::Pending,
            "{option} should wait for an active download restart"
        );
        assert_eq!(
            is_option_changeable(option, false),
            ChangeableKind::Immediate,
            "{option} should apply immediately to a reserved download"
        );
    }
    #[cfg(feature = "bittorrent")]
    {
        assert_eq!(
            is_option_changeable("bt-max-upload-slots", true),
            ChangeableKind::Pending
        );
        assert_eq!(
            is_option_changeable("bt-max-upload-slots", false),
            ChangeableKind::Immediate
        );
        assert_eq!(
            is_option_changeable("bt-optimistic-unchoke-interval", true),
            ChangeableKind::Pending
        );
        assert_eq!(
            is_option_changeable("bt-optimistic-unchoke-interval", false),
            ChangeableKind::Immediate
        );
        assert_eq!(
            is_option_changeable("bt-snubbed-timeout", true),
            ChangeableKind::Pending
        );
        assert_eq!(
            is_option_changeable("bt-snubbed-timeout", false),
            ChangeableKind::Immediate
        );
    }
    #[cfg(not(feature = "bittorrent"))]
    assert_eq!(
        is_option_changeable("bt-max-upload-slots", true),
        ChangeableKind::NotChangeable
    );
    assert_eq!(
        is_option_changeable("dir", false),
        ChangeableKind::Immediate
    );
    assert_eq!(
        is_option_changeable("show-files", false),
        ChangeableKind::NotChangeable
    );
    assert_eq!(
        is_option_changeable("max-retries", false),
        ChangeableKind::Immediate
    );
    assert_eq!(
        is_option_changeable("bt-force-encrypt", false),
        ChangeableKind::NotChangeable
    );
    assert_eq!(
        is_option_changeable("bt-detach-seed-only", false),
        ChangeableKind::NotChangeable
    );
}

#[cfg(feature = "bittorrent")]
#[test]
fn dht_engine_options_are_not_changeable_per_download() {
    for name in [
        "enable-dht",
        "enable-dht6",
        "dht-listen-port",
        "dht-listen-addr",
        "dht-listen-addr6",
        "dht-entry-point",
        "dht-entry-point-host",
        "dht-entry-point-port",
        "dht-entry-point6",
        "dht-entry-point-host6",
        "dht-entry-point-port6",
        "dht-file-path",
        "dht-file-path6",
        "dht-message-timeout",
        "dht-refresh-check-interval",
        "dht-token-rotation-interval",
        "dht-node-contact-interval",
        "dht-cleanup-interval",
        "dht-save-interval",
        "dht-bootstrap-timeout",
        "dht-max-concurrent-lookups",
        "dht-persistence-max-age",
    ] {
        assert_eq!(
            is_option_changeable(name, false),
            ChangeableKind::NotChangeable,
            "DHT process option '{name}' must not be changeable on a reserved task"
        );
        assert_eq!(
            is_option_changeable(name, true),
            ChangeableKind::NotChangeable,
            "DHT process option '{name}' must not be pending on an active task"
        );
    }
}
