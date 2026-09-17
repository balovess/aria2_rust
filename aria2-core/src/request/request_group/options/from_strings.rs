use super::{DEFAULT_DISK_CACHE_BYTES, DownloadOptions, FollowMode};
use crate::config::{OptionRegistry, OptionValue};

fn parse_list_option(
    options: &std::collections::HashMap<String, String>,
    key: &str,
) -> Option<Vec<String>> {
    let entries = options
        .get(key)?
        .split([',', '\n'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!entries.is_empty()).then_some(entries)
}

impl DownloadOptions {
    /// Build per-download options from aria2's kebab-case option map.
    ///
    /// This is the shared conversion seam for CLI/session/FFI callers. The
    /// input intentionally contains strings because that is the wire format
    /// used by aria2 configuration files and the original C++ API. Invalid
    /// values fall back to the type's default, while validation of user-facing
    /// configuration remains the responsibility of `ConfigManager`.
    pub fn from_option_strings(options: &std::collections::HashMap<String, String>) -> Self {
        let mut canonical_options = std::collections::HashMap::with_capacity(options.len());
        for (key, value) in options {
            let canonical_key = OptionRegistry::canonical_name(key).to_string();
            if key == &canonical_key || !canonical_options.contains_key(&canonical_key) {
                canonical_options.insert(canonical_key, value.clone());
            }
        }
        let options = &canonical_options;

        let positive_u16 = |key: &str| {
            options
                .get(key)
                .and_then(|v| v.parse::<u16>().ok())
                .filter(|value| *value > 0)
        };
        let positive_size_u64 = |key: &str| {
            options
                .get(key)
                .map(|v| OptionValue::parse_size_str(v))
                .filter(|value| *value > 0)
        };
        let positive_u64 = |key: &str| {
            options
                .get(key)
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|value| *value > 0)
        };

        Self {
            split: positive_u16("split"),
            force_sequential: options
                .get("force-sequential")
                .map(|v| v == "true")
                .unwrap_or(false),
            max_connection_per_server: positive_u16("max-connection-per-server"),
            max_download_limit: positive_size_u64("max-download-limit"),
            max_upload_limit: positive_size_u64("max-upload-limit"),
            dir: options.get("dir").cloned(),
            out: options.get("out").cloned(),
            disk_cache: options
                .get("disk-cache")
                .and_then(|value| OptionValue::parse_size_str_checked(value).ok())
                .or(Some(DEFAULT_DISK_CACHE_BYTES)),
            file_allocation: options.get("file-allocation").cloned(),
            allow_piece_length_change: options
                .get("allow-piece-length-change")
                .map(|v| v == "true")
                .unwrap_or(false),
            async_dns: options
                .get("async-dns")
                .map(|v| v != "false")
                .unwrap_or(true),
            continue_download: options
                .get("continue")
                .map(|v| v == "true")
                .unwrap_or(false),
            allow_overwrite: options
                .get("allow-overwrite")
                .map(|v| v == "true")
                .unwrap_or(false),
            auto_file_renaming: options
                .get("auto-file-renaming")
                .map(|v| v == "true")
                .unwrap_or(true),
            always_resume: options
                .get("always-resume")
                .map(|v| v == "true")
                .unwrap_or(true),
            max_resume_failure_tries: options
                .get("max-resume-failure-tries")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0),
            remove_control_file: options
                .get("remove-control-file")
                .map(|v| v == "true")
                .unwrap_or(false),
            mmap_threshold: positive_size_u64("mmap-threshold"),
            enable_mmap: options
                .get("enable-mmap")
                .map(|v| v == "true")
                .unwrap_or(false),
            max_mmap_limit: options
                .get("max-mmap-limit")
                .map(|v| OptionValue::parse_size_str(v)),
            no_file_allocation_limit: options
                .get("no-file-allocation-limit")
                .map(|v| OptionValue::parse_size_str(v)),
            secure_falloc: options
                .get("secure-falloc")
                .map(|v| v == "true")
                .unwrap_or(false),
            check_integrity: options
                .get("check-integrity")
                .map(|v| v == "true")
                .unwrap_or(false)
                || options
                    .get("hash-check-only")
                    .map(|v| v == "true")
                    .unwrap_or(false),
            hash_check_only: options
                .get("hash-check-only")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_enable_hook_after_hash_check: options
                .get("bt-enable-hook-after-hash-check")
                .map(|v| v == "true")
                .unwrap_or(true),
            bt_hash_check_seed: options
                .get("bt-hash-check-seed")
                .map(|v| v == "true")
                .unwrap_or(true),
            bt_seed_unverified: options
                .get("bt-seed-unverified")
                .map(|v| v == "true")
                .unwrap_or(false),
            seed_time: options.get("seed-time").and_then(|v| v.parse::<f64>().ok()),
            seed_ratio: options
                .get("seed-ratio")
                .and_then(|v| v.parse::<f64>().ok())
                .or(Some(1.0)),
            checksum: options.get("checksum").and_then(|v| {
                v.split_once('=')
                    .map(|(algo, hash)| (algo.trim().to_string(), hash.trim().to_string()))
            }),
            cookie_file: options
                .get("load-cookies")
                .or_else(|| options.get("cookie-file"))
                .cloned(),
            cookies: options
                .get("cookie")
                .or_else(|| options.get("cookies"))
                .cloned(),
            bt_max_peers: options
                .get("bt-max-peers")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(55),
            bt_force_encrypt: options
                .get("bt-force-encrypt")
                .or_else(|| options.get("bt-force-encryption"))
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_require_crypto: options
                .get("bt-require-crypto")
                .map(|v| v == "true")
                .unwrap_or(false),
            enable_dht: options
                .get("enable-dht")
                .map(|v| v != "false")
                .unwrap_or(true),
            dht_listen_port: options.get("dht-listen-port").cloned(),
            index_out: options.get("index-out").cloned(),
            dht_entry_point: parse_list_option(options, "dht-entry-point"),
            bt_tracker: parse_list_option(options, "bt-tracker"),
            bt_exclude_tracker: parse_list_option(options, "bt-exclude-tracker"),
            bt_external_ip: options.get("bt-external-ip").cloned(),
            bt_load_saved_metadata: options
                .get("bt-load-saved-metadata")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_metadata_only: options
                .get("bt-metadata-only")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_min_crypto_level: options
                .get("bt-min-crypto-level")
                .cloned()
                .unwrap_or_else(|| "plain".to_string()),
            bt_request_peer_speed_limit: options
                .get("bt-request-peer-speed-limit")
                .map(|v| OptionValue::parse_size_str(v))
                .unwrap_or(50 * 1024),
            bt_save_metadata: options
                .get("bt-save-metadata")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_enable_web_seed: options
                .get("bt-enable-web-seed")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_max_open_files: options
                .get("bt-max-open-files")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(100),
            bt_peer_blocklist: options.get("bt-peer-blocklist").cloned(),
            bt_keep_alive_interval: options
                .get("bt-keep-alive-interval")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(120),
            bt_timeout: options
                .get("bt-timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(180),
            bt_request_timeout: options
                .get("bt-request-timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(60),
            peer_connection_timeout: options
                .get("peer-connection-timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(20),
            peer_id_prefix: options
                .get("peer-id-prefix")
                .cloned()
                .unwrap_or_else(|| aria2_protocol::identity::DEFAULT_PEER_ID_PREFIX.to_string()),
            peer_agent: options
                .get("peer-agent")
                .cloned()
                .unwrap_or_else(|| aria2_protocol::identity::DEFAULT_PEER_AGENT.to_string()),
            dht_message_timeout: options
                .get("dht-message-timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(10),
            enable_dht6: options
                .get("enable-dht6")
                .map(|v| v == "true")
                .unwrap_or(false),
            dht_listen_addr6: options.get("dht-listen-addr6").cloned(),
            dht_entry_point_host: options.get("dht-entry-point-host").cloned(),
            dht_entry_point_port: options
                .get("dht-entry-point-port")
                .and_then(|v| v.parse::<u16>().ok()),
            dht_entry_point6: options.get("dht-entry-point6").cloned(),
            dht_entry_point_host6: options.get("dht-entry-point-host6").cloned(),
            dht_entry_point_port6: options
                .get("dht-entry-point-port6")
                .and_then(|v| v.parse::<u16>().ok()),
            dht_file_path6: options.get("dht-file-path6").cloned(),
            dht_listen_addr: options.get("dht-listen-addr").cloned(),
            bt_tracker_interval: options
                .get("bt-tracker-interval")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0),
            bt_tracker_connect_timeout: options
                .get("bt-tracker-connect-timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60),
            bt_tracker_timeout: options
                .get("bt-tracker-timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60),
            bt_tracker_stopped_timeout: options
                .get("bt-tracker-stopped-timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS),
            enable_peer_exchange: options
                .get("enable-peer-exchange")
                .map(|v| v != "false")
                .unwrap_or(true),
            enable_public_trackers: options
                .get("enable-public-trackers")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_piece_selection_strategy: options
                .get("bt-piece-selection-strategy")
                .cloned()
                .unwrap_or_else(|| crate::constants::DEFAULT_PIECE_STRATEGY.to_string()),
            bt_endgame_threshold: options
                .get("bt-endgame-threshold")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(crate::constants::DEFAULT_BT_ENDGAME_THRESHOLD as u32),
            max_retries: options
                .get("max-tries")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(crate::constants::DEFAULT_MAX_RETRIES),
            retry_wait: options
                .get("retry-wait")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(crate::constants::DEFAULT_RETRY_WAIT_SECS),
            http_proxy: options.get("http-proxy").cloned(),
            http_proxy_user: options.get("http-proxy-user").cloned(),
            http_proxy_passwd: options.get("http-proxy-passwd").cloned(),
            all_proxy: options.get("all-proxy").cloned(),
            all_proxy_user: options.get("all-proxy-user").cloned(),
            all_proxy_passwd: options.get("all-proxy-passwd").cloned(),
            https_proxy: options.get("https-proxy").cloned(),
            https_proxy_user: options.get("https-proxy-user").cloned(),
            https_proxy_passwd: options.get("https-proxy-passwd").cloned(),
            ftp_proxy: options.get("ftp-proxy").cloned(),
            ftp_proxy_user: options.get("ftp-proxy-user").cloned(),
            ftp_proxy_passwd: options.get("ftp-proxy-passwd").cloned(),
            no_proxy: options.get("no-proxy").cloned(),
            dht_file_path: options.get("dht-file-path").cloned(),
            bt_max_upload_slots: options
                .get("bt-max-upload-slots")
                .and_then(|v| v.parse::<u32>().ok()),
            bt_optimistic_unchoke_interval: options
                .get("bt-optimistic-unchoke-interval")
                .and_then(|v| v.parse::<u64>().ok()),
            bt_snubbed_timeout: options
                .get("bt-snubbed-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            bt_prioritize_piece: options
                .get("bt-prioritize-piece")
                .cloned()
                .unwrap_or_default(),
            bt_detach_seed_only: options
                .get("bt-detach-seed-only")
                .map(|v| v == "true")
                .unwrap_or(false),
            enable_utp: options
                .get("enable-utp")
                .map(|v| v == "true")
                .unwrap_or(false),
            utp_listen_port: positive_u16("utp-listen-port"),
            header: parse_list_option(options, "header").unwrap_or_default(),
            user_agent: options.get("user-agent").cloned(),
            referer: options.get("referer").cloned(),
            enable_http_keep_alive: options
                .get("enable-http-keep-alive")
                .map(|v| v != "false")
                .unwrap_or(true),
            enable_http_pipelining: options
                .get("enable-http-pipelining")
                .map(|v| v == "true")
                .unwrap_or(false),
            http_accept_gzip: options
                .get("http-accept-gzip")
                .map(|v| v == "true")
                .unwrap_or(false),
            http_no_cache: options
                .get("http-no-cache")
                .map(|v| v == "true")
                .unwrap_or(false),
            use_head: options
                .get("use-head")
                .map(|v| v == "true")
                .unwrap_or(false),
            no_want_digest_header: options
                .get("no-want-digest-header")
                .map(|v| v == "true")
                .unwrap_or(false),
            check_certificate: options
                .get("check-certificate")
                .map(|v| v != "false")
                .unwrap_or(true),
            ca_certificate: options.get("ca-certificate").cloned(),
            certificate: options.get("certificate").cloned(),
            private_key: options.get("private-key").cloned(),
            min_tls_version: options.get("min-tls-version").cloned(),
            metalink_version: options.get("metalink-version").cloned(),
            metalink_language: options.get("metalink-language").cloned(),
            metalink_os: options.get("metalink-os").cloned(),
            metalink_location: options.get("metalink-location").cloned(),
            metalink_preferred_protocol: options.get("metalink-preferred-protocol").cloned(),
            metalink_base_uri: options.get("metalink-base-uri").cloned(),
            select_file: options.get("select-file").cloned(),
            bt_remove_unselected_file: options
                .get("bt-remove-unselected-file")
                .map(|v| v == "true")
                .unwrap_or(false),
            piece_length: positive_size_u64("piece-length"),
            metalink_enable_unique_protocol: options
                .get("metalink-enable-unique-protocol")
                .map(|v| v != "false")
                .unwrap_or(true),
            min_split_size: options
                .get("min-split-size")
                .map(|v| OptionValue::parse_size_str(v))
                .filter(|value| *value > 0)
                .or(Some(crate::constants::DEFAULT_MIN_SPLIT_SIZE)),
            parameterized_uri: options
                .get("parameterized-uri")
                .map(|v| v == "true")
                .unwrap_or(false),
            reuse_uri: options
                .get("reuse-uri")
                .map(|v| v != "false")
                .unwrap_or(true),
            uri_selector: options
                .get("uri-selector")
                .cloned()
                .unwrap_or_else(|| "feedback".to_string()),
            stream_piece_selector: options
                .get("stream-piece-selector")
                .cloned()
                .unwrap_or_else(|| "default".to_string()),
            timeout: positive_u64("timeout"),
            connect_timeout: positive_u64("connect-timeout"),
            startup_idle_time: positive_u64("startup-idle-time"),
            lowest_speed_limit: positive_size_u64("lowest-speed-limit"),
            ftp_pasv: options
                .get("ftp-pasv")
                .map(|v| v != "false")
                .unwrap_or(true),
            ftp_type: options
                .get("ftp-type")
                .cloned()
                .unwrap_or_else(|| "binary".to_string()),
            remote_time: options
                .get("remote-time")
                .map(|v| v == "true")
                .unwrap_or(false),
            dry_run: options.get("dry-run").map(|v| v == "true").unwrap_or(false),
            ftp_reuse_connection: options
                .get("ftp-reuse-connection")
                .map(|v| v != "false")
                .unwrap_or(true),
            realtime_chunk_checksum: options
                .get("realtime-chunk-checksum")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_stop_timeout: options
                .get("bt-stop-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            disable_ipv6: options
                .get("disable-ipv6")
                .map(|v| v == "true")
                .unwrap_or(false),
            listen_port: options.get("listen-port").cloned(),
            bt_enable_lpd: options
                .get("bt-enable-lpd")
                .map(|v| v == "true")
                .unwrap_or(false),
            enable_rpc: options
                .get("enable-rpc")
                .map(|v| v == "true")
                .unwrap_or(false),
            pause: options.get("pause").map(|v| v == "true").unwrap_or(false),
            pause_metadata: options
                .get("pause-metadata")
                .map(|v| v == "true")
                .unwrap_or(false),
            force_save: options
                .get("force-save")
                .map(|v| v == "true")
                .unwrap_or(false),
            save_not_found: options
                .get("save-not-found")
                .map(|v| v != "false")
                .unwrap_or(true),
            rpc_save_upload_metadata: options
                .get("rpc-save-upload-metadata")
                .map(|v| v != "false")
                .unwrap_or(true),
            content_disposition_default_utf8: options
                .get("content-disposition-default-utf8")
                .map(|v| v == "true")
                .unwrap_or(false),
            proxy_method: options
                .get("proxy-method")
                .cloned()
                .unwrap_or_else(|| "get".to_string()),
            max_file_not_found: options
                .get("max-file-not-found")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0),
            follow_torrent: options
                .get("follow-torrent")
                .and_then(|v| FollowMode::parse(v)),
            follow_metalink: options
                .get("follow-metalink")
                .and_then(|v| FollowMode::parse(v)),
            http_auth_challenge: options
                .get("http-auth-challenge")
                .map(|v| v == "true")
                .unwrap_or(false),
            http_user: options.get("http-user").cloned(),
            http_passwd: options.get("http-passwd").cloned(),
            ftp_user: options.get("ftp-user").cloned(),
            ftp_passwd: options.get("ftp-passwd").cloned(),
            ssh_host_key_md: options.get("ssh-host-key-md").cloned(),
            no_netrc: options
                .get("no-netrc")
                .map(|v| v == "true")
                .unwrap_or(false),
            netrc_path: options.get("netrc-path").cloned(),
            conditional_get: options
                .get("conditional-get")
                .map(|v| v == "true")
                .unwrap_or(false),
            on_download_start: options.get("on-download-start").cloned(),
            on_download_complete: options.get("on-download-complete").cloned(),
            on_download_error: options.get("on-download-error").cloned(),
            on_download_pause: options.get("on-download-pause").cloned(),
            on_download_stop: options.get("on-download-stop").cloned(),
            on_bt_download_complete: options.get("on-bt-download-complete").cloned(),
        }
    }
}
