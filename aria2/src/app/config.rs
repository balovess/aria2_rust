//! Configuration loading and management for the App
//!
//! This module handles all configuration-related operations:
//! - Command-line argument loading (from clap `CliArgs`)
//! - Environment variable loading
//! - Configuration file loading
//! - Option retrieval helpers

use super::App;
use super::cli::CliArgs;
use super::config_maintenance;
use aria2_core::config::{OptionValue, UriListFile, project_initial_options};
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::validation::protocol_detector::detect;
use std::path::Path;
use tracing::warn;

impl App {
    /// Validate startup configuration without initializing the download
    /// engine or requiring a positional URI.
    pub async fn check_config(
        &mut self,
        no_conf: bool,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        self.load_startup_config(no_conf, path).await
    }

    /// Disable only invalid configuration entries while retaining a backup.
    pub fn repair_config_file(path: &Path) -> Result<(std::path::PathBuf, usize), String> {
        let update = config_maintenance::repair(path)?;
        Ok((update.backup_path, update.changed_lines))
    }

    /// Replace a configuration file with the built-in defaults after backing it up.
    pub fn reset_config_file(path: &Path) -> Result<std::path::PathBuf, String> {
        let update = config_maintenance::reset(path)?;
        Ok(update.backup_path)
    }

    async fn global_option_values(&self) -> std::collections::HashMap<String, OptionValue> {
        let config = self.config.read().await;
        config.get_all_global_options().await
    }

    /// Return the typed execution options together with the canonical raw
    /// request values that must remain attached to each CLI-created
    /// `RequestGroup`.
    pub(super) async fn download_options_with_snapshot(
        &self,
    ) -> (
        DownloadOptions,
        std::collections::HashMap<String, serde_json::Value>,
    ) {
        let values = self.global_option_values().await;
        let options = DownloadOptions::from_option_values(&values);
        let snapshot = project_initial_options(
            values
                .into_iter()
                .filter(|(_, value)| !value.is_none())
                .map(|(name, value)| (name, serde_json::Value::from(&value))),
        );
        (options, snapshot)
    }

    /// Load parsed CLI arguments (from clap `CliArgs`) into the configuration.
    ///
    /// Each option that was explicitly set on the command line is applied to
    /// `ConfigManager` with the highest priority (overriding env/file/defaults).
    /// Options not present (`None` / `false` for bools) are skipped so that
    /// config-file or env values are preserved.
    ///
    /// Negation flags (`--no-check-certificate`, `--no-continue`, `--no-enable-dht`)
    /// take precedence over their positive counterparts.
    ///
    /// Positional URIs are collected from `cli.uris`, with `@file` references
    /// expanded via `UriListFile`. `--input-file` URIs are also appended.
    pub async fn load_cli_args(&mut self, cli: CliArgs) -> std::result::Result<(), String> {
        let cli_explicit_timeout = cli.http_ftp.timeout.is_some();
        let mut conf = self.config.write().await;
        self.explicit_timeout = cli_explicit_timeout
            || conf
                .global_option_source("timeout")
                .await
                .is_some_and(|source| {
                    !matches!(source, aria2_core::config::ConfigSource::Defaults)
                });

        // Scoop normally owns the download cache and may omit `dir` from its
        // generated config. Preserve that integration when the option is
        // otherwise still at the registry default; an explicit user setting
        // must continue to win.
        if conf
            .global_option_source("dir")
            .await
            .is_some_and(|source| matches!(source, aria2_core::config::ConfigSource::Defaults))
            && let Some(dir) = scoop_cache_dir()
        {
            conf.set_global_option("dir", OptionValue::Str(dir))
                .await
                .map_err(|e| format!("--dir: {}", e))?;
        }

        // Helper macros: set option only if value is present and propagate the
        // registry's validation error back to the CLI entry point.
        macro_rules! set_str {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    conf.set_global_option($name, OptionValue::Str(v))
                        .await
                        .map_err(|e| format!("--{}: {}", $name, e))?;
                }
            };
        }
        macro_rules! set_path {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    conf.set_global_option(
                        $name,
                        OptionValue::Str(v.to_string_lossy().into_owned()),
                    )
                    .await
                    .map_err(|e| format!("--{}: {}", $name, e))?;
                }
            };
        }
        macro_rules! set_u64 {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    let v = i64::try_from(v)
                        .map_err(|_| format!("--{}: value is out of range", $name))?;
                    conf.set_global_option($name, OptionValue::Int(v))
                        .await
                        .map_err(|e| format!("--{}: {}", $name, e))?;
                }
            };
        }
        macro_rules! set_u16 {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    conf.set_global_option($name, OptionValue::Int(i64::from(v)))
                        .await
                        .map_err(|e| format!("--{}: {}", $name, e))?;
                }
            };
        }
        macro_rules! set_f64 {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    conf.set_global_option($name, OptionValue::Float(v))
                        .await
                        .map_err(|e| format!("--{}: {}", $name, e))?;
                }
            };
        }
        macro_rules! set_bool_true {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    conf.set_global_option($name, OptionValue::Bool(v))
                        .await
                        .map_err(|e| format!("--{}: {}", $name, e))?;
                }
            };
        }
        macro_rules! set_bool_false {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    conf.set_global_option($name, OptionValue::Bool(!v))
                        .await
                        .map_err(|e| format!("--{}: {}", $name, e))?;
                }
            };
        }

        let g = cli.general;
        let h = cli.http_ftp;
        let b = cli.bittorrent;
        let r = cli.rpc;
        let a = cli.advanced;

        // --- General options ---
        set_path!("dir", g.dir);
        set_str!("out", g.out);
        set_path!("log", g.log);
        set_u64!("log-backup-count", g.log_backup_count);
        set_str!("log-level", g.log_level);
        set_str!("console-log-level", g.console_log_level);
        set_u64!("summary-interval", g.summary_interval);
        set_path!("input-file", g.input_file);
        set_path!("save-session", g.save_session);
        set_u64!("save-session-interval", g.save_session_interval);
        set_u64!("auto-save-interval", g.auto_save_interval);
        set_bool_true!("enable-color", g.enable_color);
        set_bool_true!("quiet", g.quiet);
        set_bool_true!("dry-run", g.dry_run);
        set_bool_true!("daemon", g.daemon);
        set_path!("pid-file", g.pid_file);
        set_bool_true!("update-check", g.update_check);
        set_u64!("update-check-interval-days", g.update_check_interval_days);
        set_bool_true!("allow-piece-length-change", g.allow_piece_length_change);
        set_bool_true!("always-resume", g.always_resume);
        set_bool_true!("check-integrity", g.check_integrity);
        set_bool_true!("conditional-get", g.conditional_get);
        set_bool_true!("deferred-input", g.deferred_input);
        set_bool_true!("disable-ipv6", g.disable_ipv6);
        set_bool_true!("hash-check-only", g.hash_check_only);
        set_str!("follow-metalink", g.follow_metalink);
        set_str!("metalink-version", g.metalink_version);
        set_str!("metalink-language", g.metalink_language);
        set_str!("metalink-os", g.metalink_os);
        set_str!("metalink-location", g.metalink_location);
        set_str!("metalink-preferred-protocol", g.metalink_preferred_protocol);
        set_bool_true!("parameterized-uri", g.parameterized_uri);
        set_bool_true!("pause", g.pause);
        set_bool_true!("remove-control-file", g.remove_control_file);
        set_bool_true!("reuse-uri", g.reuse_uri);
        set_bool_true!("save-not-found", g.save_not_found);
        set_bool_true!("force-sequential", g.force_sequential);
        set_bool_true!("no-netrc", g.no_netrc);
        set_bool_true!("realtime-chunk-checksum", g.realtime_chunk_checksum);
        set_str!("download-result", g.download_result);
        set_bool_true!("human-readable", g.human_readable);
        set_bool_true!(
            "keep-unfinished-download-result",
            g.keep_unfinished_download_result
        );
        set_bool_true!("truncate-console-readout", g.truncate_console_readout);
        set_bool_true!("stderr", g.stderr);
        set_u64!("max-download-result", g.max_download_result);
        set_str!("lowest-speed-limit", g.lowest_speed_limit);
        set_u64!("max-downloads", g.max_downloads);
        set_u64!("max-file-not-found", g.max_file_not_found);
        set_str!("no-file-allocation-limit", g.no_file_allocation_limit);
        set_u64!("stop-with-process", g.stop_with_process);
        set_str!("uri-selector", g.uri_selector);
        set_str!("stream-piece-selector", g.stream_piece_selector);
        set_str!("interface", g.interface);
        set_str!("multiple-interface", g.multiple_interface);
        set_str!("gid", g.gid);
        set_bool_true!("async-dns", g.async_dns);
        set_u64!("dns-timeout", g.dns_timeout);
        set_str!("async-dns-server", g.async_dns_server);
        set_bool_true!("enable-async-dns6", g.enable_async_dns6);
        set_str!("event-poll", g.event_poll);
        set_path!("server-stat-if", g.server_stat_if);
        set_path!("server-stat-of", g.server_stat_of);
        set_u64!("server-stat-timeout", g.server_stat_timeout);
        set_path!("netrc-path", g.netrc_path);
        set_bool_true!("show-files", g.show_files);
        set_path!("torrent-file", g.torrent_file);
        set_path!("metalink-file", g.metalink_file);
        set_str!("checksum", g.checksum);
        set_bool_true!("select-least-used-host", g.select_least_used_host);
        set_u64!("startup-idle-time", g.startup_idle_time);
        set_bool_true!("enable-mmap", g.enable_mmap);
        set_str!("max-mmap-limit", g.max_mmap_limit);
        set_bool_true!(
            "metalink-enable-unique-protocol",
            g.metalink_enable_unique_protocol
        );
        set_str!("metalink-base-uri", g.metalink_base_uri);
        set_bool_true!("pause-metadata", g.pause_metadata);
        set_str!("on-download-start", g.on_download_start);
        set_str!("on-download-stop", g.on_download_stop);
        set_str!("on-download-pause", g.on_download_pause);
        set_str!("on-download-complete", g.on_download_complete);
        set_str!("on-download-error", g.on_download_error);
        set_bool_true!("show-console-readout", g.show_console_readout);
        set_u64!("rlimit-nofile", g.rlimit_nofile);

        // --- HTTP/FTP options ---
        set_str!("all-proxy", h.all_proxy);
        set_str!("http-proxy", h.http_proxy);
        set_str!("https-proxy", h.https_proxy);
        set_str!("ftp-proxy", h.ftp_proxy);
        set_str!("all-proxy-user", h.all_proxy_user);
        set_str!("all-proxy-passwd", h.all_proxy_passwd);
        set_str!("http-proxy-user", h.http_proxy_user);
        set_str!("http-proxy-passwd", h.http_proxy_passwd);
        set_str!("https-proxy-user", h.https_proxy_user);
        set_str!("https-proxy-passwd", h.https_proxy_passwd);
        set_str!("ftp-proxy-user", h.ftp_proxy_user);
        set_str!("ftp-proxy-passwd", h.ftp_proxy_passwd);
        set_str!("proxy-method", h.proxy_method);
        set_str!("no-proxy", h.no_proxy);
        set_str!("user-agent", h.user_agent);
        set_str!("referer", h.referer);
        if !h.header.is_empty() {
            conf.set_global_option("header", OptionValue::List(h.header))
                .await
                .map_err(|e| format!("--header: {}", e))?;
        }
        set_path!("load-cookies", h.load_cookies);
        set_path!("save-cookies", h.save_cookies);
        set_u64!("connect-timeout", h.connect_timeout);
        set_u64!("timeout", h.timeout);
        set_u64!("max-tries", h.max_tries);
        set_u64!("retry-wait", h.retry_wait);
        set_u64!("split", h.split);
        set_str!("min-split-size", h.min_split_size);
        set_u64!("max-connection-per-server", h.max_connection_per_server);
        set_u64!("max-http-pipelining", h.max_http_pipelining);
        // Negation: --no-check-certificate takes precedence over --check-certificate
        if h.no_check_certificate.unwrap_or(false) {
            set_bool_false!("check-certificate", Some(true));
        } else {
            set_bool_true!("check-certificate", h.check_certificate);
        }
        set_path!("ca-certificate", h.ca_certificate);
        set_path!("certificate", h.certificate);
        set_path!("private-key", h.private_key);
        set_str!("min-tls-version", h.min_tls_version);
        set_bool_true!("allow-overwrite", h.allow_overwrite);
        set_bool_true!("auto-file-renaming", h.auto_file_renaming);
        if h.no_continue.unwrap_or(false) {
            set_bool_false!("continue", Some(true));
        } else {
            set_bool_true!("continue", h.continue_dl);
        }
        set_bool_true!("remote-time", h.remote_time);
        set_bool_true!("enable-http-keep-alive", h.enable_http_keep_alive);
        set_bool_true!("enable-http-pipelining", h.enable_http_pipelining);
        set_bool_true!("http-accept-gzip", h.http_accept_gzip);
        set_bool_true!("http-auth-challenge", h.http_auth_challenge);
        set_bool_true!("http-no-cache", h.http_no_cache);
        set_bool_true!(
            "content-disposition-default-utf8",
            h.content_disposition_default_utf8
        );
        set_bool_true!("use-head", h.use_head);
        set_bool_true!("no-want-digest-header", h.no_want_digest_header);
        set_str!("http-user", h.http_user);
        set_str!("http-passwd", h.http_passwd);
        set_str!("ftp-user", h.ftp_user);
        set_str!("ftp-passwd", h.ftp_passwd);
        set_bool_true!("ftp-pasv", h.ftp_pasv);
        set_bool_true!("ftp-reuse-connection", h.ftp_reuse_connection);
        set_str!("ftp-type", h.ftp_type);
        set_str!("ssh-host-key-md", h.ssh_host_key_md);

        // --- BitTorrent options ---
        set_f64!("seed-time", b.seed_time);
        set_f64!("seed-ratio", b.seed_ratio);
        set_u64!("bt-max-peers", b.bt_max_peers);
        set_str!("bt-request-peer-speed-limit", b.bt_request_peer_speed_limit);
        set_u64!("bt-max-open-files", b.bt_max_open_files);
        set_path!("bt-peer-blocklist", b.bt_peer_blocklist);
        set_u64!("bt-keep-alive-interval", b.bt_keep_alive_interval);
        set_u64!("bt-timeout", b.bt_timeout);
        set_u64!("bt-request-timeout", b.bt_request_timeout);
        set_u64!("peer-connection-timeout", b.peer_connection_timeout);
        set_bool_true!("bt-seed-unverified", b.bt_seed_unverified);
        set_bool_true!("bt-save-metadata", b.bt_save_metadata);
        set_bool_true!("bt-force-encryption", b.bt_force_encryption);
        set_str!("bt-min-crypto-level", b.bt_min_crypto_level);
        set_bool_true!("bt-enable-lpd", b.bt_enable_lpd.or(b.enable_lpd));
        set_u64!("lpd-listen-port", b.lpd_listen_port);
        set_bool_true!("bt-enable-web-seed", b.bt_enable_web_seed);
        if b.no_enable_dht.unwrap_or(false) {
            set_bool_false!("enable-dht", Some(true));
        } else {
            set_bool_true!("enable-dht", b.enable_dht);
        }
        set_str!("dht-listen-port", b.dht_listen_port);
        set_str!("dht-listen-addr", b.dht_listen_addr);
        set_str!("dht-entry-point", b.dht_entry_point);
        set_str!("dht-entry-point-host", b.dht_entry_point_host);
        set_u16!("dht-entry-point-port", b.dht_entry_point_port);
        set_path!("dht-file-path", b.dht_file_path.or(b.dht_message_path));
        set_bool_true!("enable-peer-exchange", b.enable_peer_exchange);
        set_str!("follow-torrent", b.follow_torrent);
        set_str!("on-bt-download-complete", b.on_bt_download_complete);
        set_str!("on-bt-download-error", b.on_bt_download_error);
        set_str!("listen-port", b.listen_port);
        set_str!("bt-prioritize-piece", b.bt_prioritize_piece);
        set_bool_true!("enable-utp", b.enable_utp);
        set_u64!("utp-listen-port", b.utp_listen_port);
        set_bool_true!("bt-detach-seed-only", b.bt_detach_seed_only);
        set_bool_true!(
            "bt-enable-hook-after-hash-check",
            b.bt_enable_hook_after_hash_check
        );
        set_str!("bt-exclude-tracker", b.bt_exclude_tracker);
        set_str!("bt-external-ip", b.bt_external_ip);
        set_bool_true!("bt-hash-check-seed", b.bt_hash_check_seed);
        set_bool_true!("bt-load-saved-metadata", b.bt_load_saved_metadata);
        set_str!("bt-lpd-interface", b.bt_lpd_interface);
        set_bool_true!("bt-metadata-only", b.bt_metadata_only);
        set_bool_true!("bt-remove-unselected-file", b.bt_remove_unselected_file);
        set_bool_true!("bt-require-crypto", b.bt_require_crypto);
        set_u64!("bt-stop-timeout", b.bt_stop_timeout);
        set_str!("bt-tracker", b.bt_tracker);
        set_bool_true!("enable-public-trackers", b.enable_public_trackers);
        set_str!("bt-tracker-source", b.bt_tracker_source);
        set_u64!("bt-tracker-update-interval", b.bt_tracker_update_interval);
        set_u64!("bt-tracker-connect-timeout", b.bt_tracker_connect_timeout);
        set_u64!("bt-tracker-interval", b.bt_tracker_interval);
        set_u64!("bt-tracker-timeout", b.bt_tracker_timeout);
        set_u64!("bt-tracker-stopped-timeout", b.bt_tracker_stopped_timeout);
        set_u64!("dht-message-timeout", b.dht_message_timeout);
        set_bool_true!("enable-dht6", b.enable_dht6);
        set_str!("dht-listen-addr6", b.dht_listen_addr6);
        set_str!("dht-entry-point6", b.dht_entry_point6);
        set_str!("dht-entry-point-host6", b.dht_entry_point_host6);
        set_u16!("dht-entry-point-port6", b.dht_entry_point_port6);
        set_path!("dht-file-path6", b.dht_file_path6);
        set_str!("peer-id-prefix", b.peer_id_prefix);
        set_str!("peer-agent", b.peer_agent);
        set_str!("select-file", b.select_file);
        if !b.index_out.is_empty() {
            conf.set_global_option("index-out", OptionValue::Str(b.index_out.join("\n")))
                .await
                .map_err(|e| format!("--index-out: {}", e))?;
        }

        // --- RPC options ---
        set_bool_true!("enable-rpc", r.enable_rpc);
        set_bool_true!("rpc-listen-all", r.rpc_listen_all);
        set_u16!("rpc-listen-port", r.rpc_listen_port);
        set_str!("rpc-listen-address", r.rpc_listen_address);
        set_str!("rpc-secret", r.rpc_secret);
        set_str!("rpc-user", r.rpc_user);
        set_str!("rpc-passwd", r.rpc_passwd);
        set_str!("rpc-allow-origin", r.rpc_allow_origin);
        set_str!("rpc-cors-domain", r.rpc_cors_domain);
        set_bool_true!("rpc-secure", r.rpc_secure);
        set_path!("rpc-certificate", r.rpc_certificate);
        set_path!("rpc-private-key", r.rpc_private_key);
        set_bool_true!("rpc-allow-origin-all", r.rpc_allow_origin_all);
        set_str!("rpc-max-request-size", r.rpc_max_request_size);
        set_bool_true!("rpc-save-upload-metadata", r.rpc_save_upload_metadata);

        // --- Advanced options ---
        set_str!("file-allocation", a.file_allocation);
        set_bool_true!("secure-falloc", a.secure_falloc);
        set_str!("mmap-threshold", a.mmap_threshold);
        set_u64!("max-concurrent-downloads", a.max_concurrent_downloads);
        set_str!("max-overall-download-limit", a.max_overall_download_limit);
        set_str!("max-download-limit", a.max_download_limit);
        set_str!("max-overall-upload-limit", a.max_overall_upload_limit);
        set_str!("max-upload-limit", a.max_upload_limit);
        set_str!("piece-length", a.piece_length);
        set_str!("disk-cache", a.disk_cache);
        set_u64!("stop", a.stop);
        set_bool_true!("force-save", a.force_save);
        set_path!("server-stat-file", a.server_stat_file);
        set_u64!("save-server-stat-interval", a.save_server_stat_interval);
        set_u64!("dscp", a.dscp);
        set_str!("socket-recv-buffer-size", a.socket_recv_buffer_size);
        set_u64!("max-resume-failure-tries", a.max_resume_failure_tries);
        set_str!("log-max-size", a.log_max_size);
        set_u64!("log-max-files", a.log_max_files);
        set_bool_true!(
            "optimize-concurrent-downloads",
            a.optimize_concurrent_downloads
        );
        set_f64!(
            "optimize-concurrent-downloads-coeffA",
            a.optimize_concurrent_downloads_coeff_a
        );
        set_f64!(
            "optimize-concurrent-downloads-coeffB",
            a.optimize_concurrent_downloads_coeff_b
        );

        drop(conf);

        // Collect positional URIs, expanding @file references
        let mut positional_uris = Vec::new();
        for uri in cli.uris {
            if let Some(path) = uri.strip_prefix('@') {
                match UriListFile::from_file(path) {
                    Ok(uri_list) => {
                        for entry in uri_list.entries() {
                            for uri in &entry.uris {
                                positional_uris.push(uri.clone());
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to load URI list file {}: {}", path, e);
                    }
                }
            } else {
                positional_uris.push(uri);
            }
        }

        // `input-file` is also the session restore path in this application.
        // A saved session starts with a URI, so treating it as a URI list here
        // would add every restored task twice.
        let input_file = self.get_opt_str("input-file").await;
        if let Some(path) = input_file
            && !Self::looks_like_session_file(&path)
        {
            match UriListFile::from_file(&path) {
                Ok(uri_list) => {
                    for entry in uri_list.entries() {
                        for uri in &entry.uris {
                            positional_uris.push(uri.clone());
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to load input-file {}: {}", path, e);
                }
            }
        }

        // `-T`/`-M` are input selectors in the original CLI, not merely
        // configuration values. Feed them through the same detector as
        // positional metadata paths so the engine receives their file data.
        for option_name in ["torrent-file", "metalink-file"] {
            if let Some(path) = self.get_opt_str(option_name).await {
                positional_uris.push(path);
            }
        }

        self.detected_inputs = positional_uris
            .into_iter()
            .map(|uri| {
                detect(&uri).map_err(|e| format!("Cannot detect input type '{}': {}", uri, e))
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        Ok(())
    }

    pub(super) fn looks_like_session_file(path: &str) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };

        // save-session may use gzip compression; ActiveSessionManager owns
        // decompression, so the magic header is enough for classification.
        if bytes.starts_with(&[0x1f, 0x8b]) {
            return true;
        }
        let content = String::from_utf8_lossy(&bytes);

        content.lines().any(|line| {
            let property = line.trim_start();
            line.starts_with([' ', '\t']) && property.starts_with("GID=")
        })
    }

    /// Load configuration from environment variables.
    pub async fn load_env(&mut self) {
        let mut conf = self.config.write().await;
        conf.load_env().await;
    }

    /// Load environment and file configuration according to CLI startup
    /// precedence. `--no-conf` suppresses both the default and explicit file.
    pub(super) async fn load_startup_config(
        &mut self,
        no_conf: bool,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        if !no_conf {
            self.load_config_file(path).await?;
        }
        self.load_env().await;
        Ok(())
    }

    /// Load configuration from a file.
    ///
    /// If no path is provided, looks for ~/.aria2/aria2.conf
    /// Matching original aria2 behavior (option_processing.cc):
    /// - `HOME` first on all platforms, then USERPROFILE, then HOMEDRIVE+HOMEPATH.
    /// - When `--conf-path` is explicitly given and file not found → error.
    /// - When default path is not found → silently skip (graceful fallback).
    pub async fn load_config_file(
        &mut self,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        let conf_path = if let Some(p) = path {
            // --conf-path explicitly given: error if file doesn't exist
            // (matches original aria2 option_processing.cc lines 254-260)
            if !std::path::Path::new(p).exists() {
                let msg = format!("Config file not found: {}", p);
                eprintln!("[-] {}", msg);
                return Err(msg);
            }
            p.to_string()
        } else {
            // Home resolution matching original aria2 util.cc getHomeDir():
            // 1. HOME (primary on all platforms)
            // 2. USERPROFILE (Windows fallback)
            // 3. HOMEDRIVE+HOMEPATH (last resort Windows fallback)
            // 4. "." (fallback if nothing works)
            let candidates = crate::app::paths::default_config_candidates();
            let Some(candidate) = candidates.into_iter().find(|candidate| candidate.exists())
            else {
                return Ok(());
            };
            candidate.to_string_lossy().into_owned()
        };

        let mut conf = self.config.write().await;
        conf.load_file(&conf_path).await;
        if conf.has_errors() {
            let details = conf
                .errors()
                .iter()
                .enumerate()
                .map(|(index, error)| {
                    if let Some(context) = conf.parser().error_context(index) {
                        format!(
                            "{}:{}: {} -> {}",
                            conf_path, context.line, context.content, error
                        )
                    } else {
                        error.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "Failed to parse config file '{}': {}",
                conf_path, details
            ));
        }
        Ok(())
    }

    /// Get a string option value.
    pub(super) async fn get_opt_str(&self, name: &str) -> Option<String> {
        self.config.read().await.get_global_str(name).await
    }

    /// Get an integer option value.
    pub(super) async fn get_opt_i64(&self, name: &str) -> Option<i64> {
        self.config.read().await.get_global_i64(name).await
    }

    /// Get an usize option value.
    pub(super) async fn get_opt_usize(&self, name: &str) -> Option<usize> {
        self.config
            .read()
            .await
            .get_global_i64(name)
            .await
            .map(|v| v as usize)
    }

    /// Get a boolean option value.
    pub(super) async fn get_opt_bool(&self, name: &str) -> Option<bool> {
        self.config.read().await.get_global_bool(name).await
    }
}

#[cfg(windows)]
fn scoop_cache_dir() -> Option<String> {
    scoop_cache_dir_from_env(std::env::var_os("SCOOP_CACHE"), std::env::var_os("SCOOP"))
}

#[cfg(not(windows))]
fn scoop_cache_dir() -> Option<String> {
    None
}

#[cfg(any(windows, test))]
fn scoop_cache_dir_from_env(
    cache: Option<std::ffi::OsString>,
    scoop_root: Option<std::ffi::OsString>,
) -> Option<String> {
    let cache = cache
        .map(std::path::PathBuf::from)
        .or_else(|| scoop_root.map(|root| std::path::PathBuf::from(root).join("cache")))?;
    Some(cache.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::scoop_cache_dir_from_env;
    use std::ffi::OsString;

    #[test]
    fn scoop_cache_prefers_explicit_cache_variable() {
        assert_eq!(
            scoop_cache_dir_from_env(
                Some(OsString::from(r"D:\scoop-cache")),
                Some(OsString::from(r"D:\scoop")),
            ),
            Some(r"D:\scoop-cache".to_string())
        );
    }

    #[test]
    fn scoop_cache_falls_back_to_scoop_root() {
        let expected = std::path::PathBuf::from(r"D:\scoop")
            .join("cache")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            scoop_cache_dir_from_env(None, Some(OsString::from(r"D:\scoop"))),
            Some(expected)
        );
    }

    #[test]
    fn scoop_cache_is_disabled_without_scoop_environment() {
        assert_eq!(scoop_cache_dir_from_env(None, None), None);
    }
}
