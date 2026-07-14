//! Configuration loading and management for the App
//!
//! This module handles all configuration-related operations:
//! - Command-line argument loading (from clap `CliArgs`)
//! - Environment variable loading
//! - Configuration file loading
//! - Option retrieval helpers

use super::App;
use super::cli::CliArgs;
use aria2_core::config::{OptionValue, UriListFile};
use aria2_core::validation::protocol_detector::detect;
use tracing::warn;

impl App {
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
        let mut conf = self.config.write().await;

        // Helper macros: set option only if value is present; ignore unknown
        // options (they may be CLI-only flags like verbose/no-color not in registry)
        macro_rules! set_str {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    let _ = conf.set_global_option($name, OptionValue::Str(v)).await;
                }
            };
        }
        macro_rules! set_path {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    let _ = conf
                        .set_global_option(
                            $name,
                            OptionValue::Str(v.to_string_lossy().into_owned()),
                        )
                        .await;
                }
            };
        }
        macro_rules! set_u64 {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    let _ = conf
                        .set_global_option($name, OptionValue::Int(v as i64))
                        .await;
                }
            };
        }
        macro_rules! set_u16 {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    let _ = conf
                        .set_global_option($name, OptionValue::Int(v as i64))
                        .await;
                }
            };
        }
        macro_rules! set_f64 {
            ($name:expr, $value:expr) => {
                if let Some(v) = $value {
                    let _ = conf.set_global_option($name, OptionValue::Float(v)).await;
                }
            };
        }
        macro_rules! set_bool_true {
            ($name:expr, $value:expr) => {
                if $value {
                    let _ = conf.set_global_option($name, OptionValue::Bool(true)).await;
                }
            };
        }
        macro_rules! set_bool_false {
            ($name:expr, $value:expr) => {
                if $value {
                    let _ = conf
                        .set_global_option($name, OptionValue::Bool(false))
                        .await;
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

        // --- HTTP/FTP options ---
        set_str!("all-proxy", h.all_proxy);
        set_str!("http-proxy", h.http_proxy);
        set_str!("https-proxy", h.https_proxy);
        set_str!("ftp-proxy", h.ftp_proxy);
        set_str!("no-proxy", h.no_proxy);
        set_str!("user-agent", h.user_agent);
        set_str!("referer", h.referer);
        if !h.header.is_empty() {
            let _ = conf
                .set_global_option("header", OptionValue::List(h.header))
                .await;
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
        // Negation: --no-check-certificate takes precedence over --check-certificate
        if h.no_check_certificate {
            set_bool_false!("check-certificate", true);
        } else {
            set_bool_true!("check-certificate", h.check_certificate);
        }
        set_path!("ca-certificate", h.ca_certificate);
        set_bool_true!("allow-overwrite", h.allow_overwrite);
        set_bool_true!("auto-file-renaming", h.auto_file_renaming);
        if h.no_continue {
            set_bool_false!("continue", true);
        } else {
            set_bool_true!("continue", h.continue_dl);
        }
        set_bool_true!("remote-time", h.remote_time);

        // --- BitTorrent options ---
        set_f64!("seed-time", b.seed_time);
        set_f64!("seed-ratio", b.seed_ratio);
        set_u64!("bt-max-peers", b.bt_max_peers);
        set_str!("bt-request-peer-speed-limit", b.bt_request_peer_speed_limit);
        set_u64!("bt-max-open-files", b.bt_max_open_files);
        set_bool_true!("bt-seed-unverified", b.bt_seed_unverified);
        set_bool_true!("bt-save-metadata", b.bt_save_metadata);
        set_bool_true!("bt-force-encryption", b.bt_force_encryption);
        set_str!("bt-min-crypto-level", b.bt_min_crypto_level);
        set_bool_true!("bt-enable-lpd", b.bt_enable_lpd);
        set_bool_true!("enable-lpd", b.enable_lpd);
        set_u64!("lpd-listen-port", b.lpd_listen_port);
        set_bool_true!("bt-enable-web-seed", b.bt_enable_web_seed);
        if b.no_enable_dht {
            set_bool_false!("enable-dht", true);
        } else {
            set_bool_true!("enable-dht", b.enable_dht);
        }
        set_u64!("dht-listen-port", b.dht_listen_port);
        set_str!("dht-entry-point", b.dht_entry_point);
        set_path!("dht-file-path", b.dht_file_path);
        set_path!("dht-message-path", b.dht_message_path);
        set_bool_true!("enable-peer-exchange", b.enable_peer_exchange);
        set_str!("follow-torrent", b.follow_torrent);
        set_str!("on-bt-download-complete", b.on_bt_download_complete);
        set_str!("on-bt-download-error", b.on_bt_download_error);
        set_str!("listen-port", b.listen_port);
        set_str!("bt-prioritize-piece", b.bt_prioritize_piece);
        set_bool_true!("enable-utp", b.enable_utp);
        set_u64!("utp-listen-port", b.utp_listen_port);

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

        // Append URIs from --input-file
        let input_file = self.get_opt_str("input-file").await;
        if let Some(path) = input_file {
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

        self.detected_inputs = positional_uris
            .into_iter()
            .map(|uri| {
                detect(&uri).map_err(|e| format!("Cannot detect input type '{}': {}", uri, e))
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        Ok(())
    }

    /// Load configuration from environment variables.
    pub async fn load_env(&mut self) {
        let mut conf = self.config.write().await;
        conf.load_env().await;
    }

    /// Load configuration from a file.
    ///
    /// If no path is provided, looks for ~/.aria2/aria2.conf
    pub async fn load_config_file(
        &mut self,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        let conf_path = if let Some(p) = path {
            p.to_string()
        } else {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| ".".to_string());

            let candidate = format!(
                "{}/{}/{}",
                home,
                crate::constants::CONFIG_DIR_NAME,
                crate::constants::CONFIG_FILE_NAME
            );
            if std::path::Path::new(&candidate).exists() {
                candidate
            } else {
                return Ok(());
            }
        };

        let mut conf = self.config.write().await;
        conf.load_file(&conf_path).await;
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
