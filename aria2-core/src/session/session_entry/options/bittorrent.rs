use super::DownloadOptions;
use std::collections::HashMap;

pub(super) fn apply(opts: &DownloadOptions, map: &mut HashMap<String, String>) {
    // --- BitTorrent options ---
    if opts.bt_force_encrypt {
        map.insert("bt-force-encrypt".to_string(), "true".to_string());
    }
    if opts.bt_require_crypto {
        map.insert("bt-require-crypto".to_string(), "true".to_string());
    }
    if opts.bt_max_peers != 55 {
        map.insert("bt-max-peers".to_string(), opts.bt_max_peers.to_string());
    }
    if let Some(ref v) = opts.bt_exclude_tracker {
        map.insert("bt-exclude-tracker".to_string(), v.join(","));
    }
    if let Some(ref v) = opts.bt_external_ip {
        map.insert("bt-external-ip".to_string(), v.clone());
    }
    if opts.bt_load_saved_metadata {
        map.insert("bt-load-saved-metadata".to_string(), "true".to_string());
    } else {
        map.insert("bt-load-saved-metadata".to_string(), "false".to_string());
    }
    if opts.bt_metadata_only {
        map.insert("bt-metadata-only".to_string(), "true".to_string());
    }
    if opts.bt_min_crypto_level != "plain" {
        map.insert(
            "bt-min-crypto-level".to_string(),
            opts.bt_min_crypto_level.clone(),
        );
    }
    if opts.bt_request_peer_speed_limit != 50 * 1024 {
        map.insert(
            "bt-request-peer-speed-limit".to_string(),
            opts.bt_request_peer_speed_limit.to_string(),
        );
    }
    if opts.bt_save_metadata {
        map.insert("bt-save-metadata".to_string(), "true".to_string());
    } else {
        map.insert("bt-save-metadata".to_string(), "false".to_string());
    }
    if !opts.bt_enable_web_seed {
        map.insert("bt-enable-web-seed".to_string(), "false".to_string());
    }
    if opts.bt_max_open_files != 100 {
        map.insert(
            "bt-max-open-files".to_string(),
            opts.bt_max_open_files.to_string(),
        );
    }
    if let Some(ref v) = opts.bt_peer_blocklist {
        map.insert("bt-peer-blocklist".to_string(), v.clone());
    }
    if opts.bt_keep_alive_interval != 120 {
        map.insert(
            "bt-keep-alive-interval".to_string(),
            opts.bt_keep_alive_interval.to_string(),
        );
    }
    if opts.bt_timeout != 180 {
        map.insert("bt-timeout".to_string(), opts.bt_timeout.to_string());
    }
    if opts.bt_request_timeout != 60 {
        map.insert(
            "bt-request-timeout".to_string(),
            opts.bt_request_timeout.to_string(),
        );
    }
    if opts.peer_connection_timeout != 20 {
        map.insert(
            "peer-connection-timeout".to_string(),
            opts.peer_connection_timeout.to_string(),
        );
    }
    if opts.peer_id_prefix != aria2_protocol::identity::DEFAULT_PEER_ID_PREFIX {
        map.insert("peer-id-prefix".to_string(), opts.peer_id_prefix.clone());
    }
    if opts.peer_agent != aria2_protocol::identity::DEFAULT_PEER_AGENT {
        map.insert("peer-agent".to_string(), opts.peer_agent.clone());
    }
    if opts.dht_message_timeout != 10 {
        map.insert(
            "dht-message-timeout".to_string(),
            opts.dht_message_timeout.to_string(),
        );
    }
    for (name, value, default) in [
        (
            "dht-refresh-check-interval",
            opts.dht_refresh_check_interval,
            300,
        ),
        (
            "dht-token-rotation-interval",
            opts.dht_token_rotation_interval,
            600,
        ),
        (
            "dht-node-contact-interval",
            opts.dht_node_contact_interval,
            900,
        ),
        ("dht-cleanup-interval", opts.dht_cleanup_interval, 300),
        ("dht-save-interval", opts.dht_save_interval, 1800),
        ("dht-bootstrap-timeout", opts.dht_bootstrap_timeout, 60),
        (
            "dht-persistence-max-age",
            opts.dht_persistence_max_age,
            24 * 60 * 60,
        ),
    ] {
        if value != default {
            map.insert(name.to_string(), value.to_string());
        }
    }
    if opts.dht_max_concurrent_lookups != 16 {
        map.insert(
            "dht-max-concurrent-lookups".to_string(),
            opts.dht_max_concurrent_lookups.to_string(),
        );
    }
    if opts.enable_dht6 {
        map.insert("enable-dht6".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.dht_listen_addr6 {
        map.insert("dht-listen-addr6".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_entry_point_host {
        map.insert("dht-entry-point-host".to_string(), v.clone());
    }
    if let Some(v) = opts.dht_entry_point_port {
        map.insert("dht-entry-point-port".to_string(), v.to_string());
    }
    if let Some(ref v) = opts.dht_entry_point6 {
        map.insert("dht-entry-point6".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_entry_point_host6 {
        map.insert("dht-entry-point-host6".to_string(), v.clone());
    }
    if let Some(v) = opts.dht_entry_point_port6 {
        map.insert("dht-entry-point-port6".to_string(), v.to_string());
    }
    if let Some(ref v) = opts.dht_file_path6 {
        map.insert("dht-file-path6".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_listen_addr {
        map.insert("dht-listen-addr".to_string(), v.clone());
    }
    // enable_dht defaults to true; only save if disabled
    if !opts.enable_dht {
        map.insert("enable-dht".to_string(), "false".to_string());
    }
    if let Some(ref v) = opts.dht_listen_port {
        map.insert("dht-listen-port".to_string(), v.clone());
    }
    if let Some(ref v) = opts.listen_port {
        map.insert("listen-port".to_string(), v.clone());
    }
    if let Some(ref v) = opts.index_out {
        map.insert("index-out".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_entry_point {
        map.insert("dht-entry-point".to_string(), v.join(","));
    }
    if let Some(ref v) = opts.bt_tracker {
        map.insert("bt-tracker".to_string(), v.join(","));
    }
    if opts.bt_tracker_connect_timeout != 60 {
        map.insert(
            "bt-tracker-connect-timeout".to_string(),
            opts.bt_tracker_connect_timeout.to_string(),
        );
    }
    if opts.bt_tracker_interval != 0 {
        map.insert(
            "bt-tracker-interval".to_string(),
            opts.bt_tracker_interval.to_string(),
        );
    }
    if opts.bt_tracker_timeout != 60 {
        map.insert(
            "bt-tracker-timeout".to_string(),
            opts.bt_tracker_timeout.to_string(),
        );
    }
    if opts.bt_tracker_stopped_timeout != crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS {
        map.insert(
            "bt-tracker-stopped-timeout".to_string(),
            opts.bt_tracker_stopped_timeout.to_string(),
        );
    }
    if !opts.enable_peer_exchange {
        map.insert("enable-peer-exchange".to_string(), "false".to_string());
    }
    // enable_public_trackers defaults to true; only save if disabled
    if !opts.enable_public_trackers {
        map.insert("enable-public-trackers".to_string(), "false".to_string());
    }
    if !opts.bt_piece_selection_strategy.is_empty() {
        map.insert(
            "bt-piece-selection-strategy".to_string(),
            opts.bt_piece_selection_strategy.clone(),
        );
    }
    if opts.bt_endgame_threshold > 0 {
        map.insert(
            "bt-endgame-threshold".to_string(),
            opts.bt_endgame_threshold.to_string(),
        );
    }
    if let Some(v) = opts.bt_max_upload_slots {
        map.insert("bt-max-upload-slots".to_string(), v.to_string());
    }
    if let Some(v) = opts.bt_optimistic_unchoke_interval {
        map.insert("bt-optimistic-unchoke-interval".to_string(), v.to_string());
    }
    if let Some(v) = opts.bt_snubbed_timeout {
        map.insert("bt-snubbed-timeout".to_string(), v.to_string());
    }
    if !opts.bt_prioritize_piece.is_empty() {
        map.insert(
            "bt-prioritize-piece".to_string(),
            opts.bt_prioritize_piece.clone(),
        );
    }
    if opts.bt_detach_seed_only {
        map.insert("bt-detach-seed-only".to_string(), "true".to_string());
    }
    if opts.enable_utp {
        map.insert("enable-utp".to_string(), "true".to_string());
    }
    if let Some(v) = opts.utp_listen_port {
        map.insert("utp-listen-port".to_string(), v.to_string());
    }
}
