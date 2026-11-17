use super::*;
use aria2_core::config::ConfigManager;
use aria2_core::engine::engine_command::{EngineCommand, channel};
use aria2_core::request::request_group_man::RequestGroupMan;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::test]
async fn dht_process_options_are_ignored_by_global_runtime_changes() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "dht-message-timeout".to_string(),
            serde_json::json!(23),
        )]))
        .await
        .expect("unsupported global runtime changes should be ignored");

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("dht-message-timeout")
            .await,
        Some(10),
        "DHT process configuration must retain its configured value"
    );
    assert!(receiver.try_recv().is_err());
}

#[cfg(not(feature = "sftp"))]
#[tokio::test]
async fn sftp_options_are_ignored_by_global_runtime_changes_without_feature() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "sftp-connect-timeout".to_string(),
            serde_json::json!(41),
        )]))
        .await
        .expect("unsupported global runtime changes should be ignored");

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("sftp-connect-timeout")
            .await,
        None,
        "SFTP options must not be stored without an SFTP consumer"
    );
    assert!(receiver.try_recv().is_err());
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn global_dht_storage_limits_update_config_and_engine() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "dht-max-peer-info-hashes".to_string(),
            serde_json::json!(128),
        )]))
        .await
        .expect("DHT resource bounds are global runtime options");

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("dht-max-peer-info-hashes")
            .await,
        Some(128)
    );
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetDhtPeerStorageConfig { config }
            if config.max_info_hashes == 128
                && config.max_peers_per_info_hash
                    == aria2_protocol::bittorrent::dht::DhtPeerStorageConfig::default()
                        .max_peers_per_info_hash
    ));
}

#[tokio::test]
async fn global_dht_item_limit_dispatches_to_runtime() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "dht-max-items".to_string(),
            serde_json::json!(512),
        )]))
        .await
        .expect("DHT item capacity is a process global option");

    assert_eq!(
        config.read().await.get_global_i64("dht-max-items").await,
        Some(512)
    );
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetDhtItemStoreLimit { max_items: 512 }
    ));
}

#[tokio::test]
async fn global_option_alias_updates_canonical_concurrency_setting() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "max-downloads".to_string(),
            serde_json::json!(13),
        )]))
        .await
        .expect("registered compatibility alias should be accepted");

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("max-concurrent-downloads")
            .await,
        Some(13)
    );
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetMaxConcurrent { max: 13 }
    ));
}

#[tokio::test]
async fn global_handshake_timeout_updates_config_and_engine() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "bt-incoming-peer-handshake-timeout".to_string(),
            serde_json::json!(42),
        )]))
        .await
        .unwrap();

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("bt-incoming-peer-handshake-timeout")
            .await,
        Some(42)
    );
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetIncomingPeerHandshakeTimeout { seconds: 42 }
    ));
}

#[tokio::test]
async fn invalid_global_handshake_timeout_rejects_entire_change_batch() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );
    let original_max_concurrent = config
        .read()
        .await
        .get_global_i64("max-concurrent-downloads")
        .await;

    assert!(
        backend
            .change_global_option(HashMap::from([
                ("max-concurrent-downloads".to_string(), serde_json::json!(9),),
                (
                    "bt-incoming-peer-handshake-timeout".to_string(),
                    serde_json::json!(0),
                ),
            ]))
            .await
            .is_err()
    );
    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("bt-incoming-peer-handshake-timeout")
            .await,
        Some(30)
    );
    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("max-concurrent-downloads")
            .await,
        original_max_concurrent,
        "a rejected batch must not apply its valid sibling option"
    );
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn global_utp_options_update_config_and_shared_listener_policy() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([
            ("bt-utp-max-connections".to_string(), serde_json::json!(73)),
            ("bt-utp-connect-timeout".to_string(), serde_json::json!(14)),
            ("bt-utp-idle-timeout".to_string(), serde_json::json!(46)),
            (
                "bt-utp-keepalive-interval".to_string(),
                serde_json::json!(19),
            ),
        ]))
        .await
        .unwrap();

    let expected = aria2_protocol::bittorrent::utp::UtpSocketOptions::default()
        .with_max_connections(73)
        .with_connect_timeout(std::time::Duration::from_secs(14))
        .with_idle_timeout(std::time::Duration::from_secs(46))
        .with_keepalive_interval(std::time::Duration::from_secs(19));
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetUtpSocketOptions { options } if options == expected
    ));
    for (name, expected) in [
        ("bt-utp-max-connections", 73),
        ("bt-utp-connect-timeout", 14),
        ("bt-utp-idle-timeout", 46),
        ("bt-utp-keepalive-interval", 19),
    ] {
        assert_eq!(
            config.read().await.get_global_i64(name).await,
            Some(expected)
        );
    }
}

#[tokio::test]
async fn global_http_and_connection_options_are_stored_from_rpc_updates() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, _receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );
    let option_names = [
        "connect-timeout",
        "http-adaptive-enabled",
        "http-adaptive-reduction-percent",
        "http-adaptive-recovery-interval",
        "http-adaptive-recovery-successes-min",
        "http-adaptive-recovery-successes-max",
        "http-adaptive-start-interval-ms",
        "http-adaptive-backoff-base",
        "http-adaptive-backoff-max",
        "http-adaptive-capacity-start-interval-ms",
        "http-adaptive-capacity-start-interval-max-ms",
        "http-adaptive-rate-recovery-successes",
        "http-adaptive-rate-min-interval-ms",
        "http-adaptive-backoff-reset-after",
    ];
    let changes = option_names
        .iter()
        .map(|name| {
            let value = if *name == "http-adaptive-enabled" {
                serde_json::json!("false")
            } else if *name == "connect-timeout" {
                serde_json::json!(47)
            } else {
                serde_json::json!(1)
            };
            ((*name).to_string(), serde_json::json!(value))
        })
        .collect();

    backend.change_global_option(changes).await.unwrap();

    let config = config.read().await;
    for name in option_names {
        if name == "http-adaptive-enabled" {
            assert_eq!(config.get_global_bool(name).await, Some(false), "{name}");
        } else {
            let expected = if name == "connect-timeout" { 47 } else { 1 };
            assert_eq!(config.get_global_i64(name).await, Some(expected), "{name}");
        }
    }
}

#[tokio::test]
async fn global_tracker_source_timeout_updates_config_and_engine_command() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "bt-tracker-source-timeout".to_string(),
            serde_json::json!(47),
        )]))
        .await
        .unwrap();

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("bt-tracker-source-timeout")
            .await,
        Some(47)
    );
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetPublicTrackerSourceTimeout { seconds: 47 }
    ));
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn global_tracker_source_concurrency_updates_config_and_engine_command() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "bt-tracker-source-concurrency".to_string(),
            serde_json::json!(2),
        )]))
        .await
        .unwrap();

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("bt-tracker-source-concurrency")
            .await,
        Some(2)
    );
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetPublicTrackerSourceConcurrency {
            max_concurrent_sources: 2
        }
    ));
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn tracker_source_concurrency_rejects_values_above_the_protocol_cap() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    assert!(
        backend
            .change_global_option(HashMap::from([(
                "bt-tracker-source-concurrency".to_string(),
                serde_json::json!(5),
            )]))
            .await
            .is_err()
    );
    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("bt-tracker-source-concurrency")
            .await,
        Some(4)
    );
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn global_bt_max_open_files_updates_config_and_engine_command() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([(
            "bt-max-open-files".to_string(),
            serde_json::json!(17),
        )]))
        .await
        .unwrap();

    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("bt-max-open-files")
            .await,
        Some(17)
    );
    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetBtMaxOpenFiles { max: 17 }
    ));
}

#[tokio::test]
async fn empty_public_tracker_sources_are_accepted_and_applied() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );
    let original_excluded_trackers = config
        .read()
        .await
        .get_global_option("bt-exclude-tracker")
        .await;

    for value in [serde_json::json!(""), serde_json::json!([])] {
        backend
            .change_global_option(HashMap::from([
                ("bt-tracker-source".to_string(), value),
                ("bt-exclude-tracker".to_string(), serde_json::json!([])),
            ]))
            .await
            .unwrap();

        assert!(matches!(
            config
                .read()
                .await
                .get_global_option("bt-tracker-source")
                .await,
            Some(aria2_core::config::OptionValue::List(sources)) if sources.is_empty()
        ));
        assert!(matches!(
            receiver.recv().await.unwrap(),
            EngineCommand::SetPublicTrackerSources { sources } if sources.is_empty()
        ));
        assert_eq!(
            config
                .read()
                .await
                .get_global_option("bt-exclude-tracker")
                .await,
            original_excluded_trackers,
            "other empty arrays retain the upstream no-op behavior"
        );
    }
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn global_option_arrays_skip_non_strings_and_ignore_non_cumulative_arrays() {
    let config = Arc::new(RwLock::new(ConfigManager::new()));
    let original_max_concurrent = config
        .read()
        .await
        .get_global_i64("max-concurrent-downloads")
        .await;
    let (sender, mut receiver) = channel();
    let backend = CoreRpcBackend::new(
        Arc::new(RequestGroupMan::new()),
        sender,
        Arc::clone(&config),
        None,
        "test",
    );

    backend
        .change_global_option(HashMap::from([
            (
                "bt-tracker-source".to_string(),
                serde_json::json!([
                    "https://tracker-one.test/announce",
                    17,
                    null,
                    "https://tracker-two.test/announce"
                ]),
            ),
            (
                "max-concurrent-downloads".to_string(),
                serde_json::json!([9]),
            ),
        ]))
        .await
        .expect("non-string array members are skipped and wrong-shaped arrays ignored");

    assert!(matches!(
        receiver.recv().await.unwrap(),
        EngineCommand::SetPublicTrackerSources { sources }
            if sources == "https://tracker-one.test/announce\nhttps://tracker-two.test/announce"
    ));
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        config
            .read()
            .await
            .get_global_i64("max-concurrent-downloads")
            .await,
        original_max_concurrent
    );
}
