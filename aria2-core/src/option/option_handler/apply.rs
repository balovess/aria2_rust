//! Applying options to download contexts.
//!
//! Implements [`OptionHandlerApply`] — a trait that extends [`OptionHandler`]
//! with methods for loading config files, applying CLI argument overrides,
//! and converting options into [`DownloadOptions`] for download commands.

use std::path::Path;

use crate::config::option::OptionValue;
use crate::request::request_group::DownloadOptions;

use super::OptionHandler;
use super::parsing::{detect_value_type, parse_kv_arg};
use super::validation::parse_config_line;

/// Extension trait for [`OptionHandler`] that provides option application methods.
///
/// Separated into a dedicated module to keep the core struct definition
/// and simple accessors in [`mod.rs`] clean.
pub trait OptionHandlerApply {
    /// Override options from raw command-line arguments.
    ///
    /// Parses common CLI patterns:
    /// - `--key=value` / `--key:value`
    /// - `--key value` (value in next arg)
    /// - `--no-key` sets boolean to false
    /// - `-o key=value` (GNU style)
    ///
    /// CLI arguments take precedence over config file values but can be
    /// overridden by explicit [`set`](OptionHandler::set) calls.
    fn apply_args(&mut self, args: &[String]);

    /// Load options from a `.aria2rc` config file.
    ///
    /// File format:
    /// ```text
    /// # Comment lines start with #
    /// key=value
    /// key="value with spaces"
    /// key=['val1', 'val2']
    /// bool-key=true
    /// number-key=42
    /// float-key=3.14
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error only if the file cannot be read (IO failure).
    fn load_config_file(&mut self, path: &Path) -> Result<(), String>;

    /// Convert current options to a [`DownloadOptions`] struct suitable for
    /// creating a download task.
    fn to_download_options(&self) -> DownloadOptions;
}

impl OptionHandlerApply for OptionHandler {
    fn apply_args(&mut self, args: &[String]) {
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];

            // Skip non-option arguments (e.g., URLs, positional args)
            if !arg.starts_with('-') || arg == "--" {
                i += 1;
                continue;
            }

            // Parse --key=value or --key:value
            if let Some((key, value)) = parse_kv_arg(arg) {
                if let Some(parsed) = detect_value_type(value.trim()) {
                    tracing::debug!(key, value = ?parsed, "CLI arg applied");
                    self.set(key, parsed);
                }
                i += 1;
                continue;
            }

            // Parse --no-key (boolean false)
            if let Some(key) = arg.strip_prefix("--no-") {
                self.set(key, OptionValue::Bool(false));
                i += 1;
                continue;
            }

            // Parse --key <next-arg> (value in next argument)
            if let Some(key) = arg.strip_prefix("--") {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    let value = &args[i + 1];
                    if let Some(parsed) = detect_value_type(value) {
                        self.set(key, parsed);
                    }
                    i += 2;
                    continue;
                } else {
                    // Flag without value: treat as boolean true
                    self.set(key, OptionValue::Bool(true));
                    i += 1;
                    continue;
                }
            }

            // Parse -o key=value
            if arg == "-o" && i + 1 < args.len() {
                let next = &args[i + 1];
                if let Some((key, value)) = next.split_once('=')
                    && let Some(parsed) = detect_value_type(value.trim())
                {
                    self.set(key, parsed);
                }
                i += 2;
                continue;
            }

            i += 1;
        }
    }

    fn load_config_file(&mut self, path: &Path) -> Result<(), String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file '{}': {}", path.display(), e))?;

        let path_display = path.display().to_string();

        for (line_num, raw_line) in content.lines().enumerate() {
            if let Some((key, value_str)) = parse_config_line(raw_line, &path_display, line_num + 1)
            {
                // Auto-detect type and set
                match detect_value_type(value_str) {
                    Some(parsed) => {
                        tracing::debug!(
                            key,
                            value = ?parsed,
                            source = %path_display,
                            "Config option loaded"
                        );
                        self.set(key, parsed);
                    }
                    None => {
                        tracing::warn!(
                            key,
                            line = line_num + 1,
                            source = %path_display,
                            "Failed to parse config value"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn to_download_options(&self) -> DownloadOptions {
        let get_usize = |key: &str| -> Option<u16> {
            let v = self.get(key).as_usize();
            if v > 0 { Some(v as u16) } else { None }
        };
        let get_u64 = |key: &str| -> Option<u64> {
            let v = self.get(key).as_usize();
            if v > 0 { Some(v as u64) } else { None }
        };
        let get_f64 = |key: &str| -> Option<f64> { self.get(key).as_f64().filter(|&v| v > 0.0) };
        let get_str = |key: &str| -> Option<String> {
            self.get(key)
                .as_str()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        };

        DownloadOptions {
            split: get_usize("split"),
            max_connection_per_server: get_usize("max-connection-per-server"),
            max_download_limit: get_u64("max-download-limit"),
            max_upload_limit: get_u64("max-upload-limit"),
            dir: get_str("dir"),
            out: get_str("out"),
            seed_time: get_f64("seed-time"),
            seed_ratio: {
                let r = self.get("seed-ratio").as_f64().unwrap_or(0.0);
                if r > 0.0 { Some(r) } else { None }
            },
            checksum: None,
            cookie_file: get_str("cookie-file"),
            cookies: get_str("cookies"),
            bt_max_peers: self.get("bt-max-peers").as_usize(),
            bt_force_encrypt: self.get("bt-force-encrypt").as_bool().unwrap_or(false),
            bt_require_crypto: self.get("bt-require-crypto").as_bool().unwrap_or(false),
            enable_dht: self.get("enable-dht").as_bool().unwrap_or(true),
            dht_listen_port: get_usize("dht-listen-port"),
            bt_tracker: {
                let v = self.get("bt-tracker").as_str().unwrap_or("");
                if v.is_empty() {
                    None
                } else {
                    Some(
                        v.split([',', '\n'])
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    )
                }
            },
            dht_entry_point: {
                let v = self.get("dht-entry-point").as_str().unwrap_or("");
                if v.is_empty() {
                    None
                } else {
                    Some(
                        v.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    )
                }
            },
            enable_public_trackers: self.get("enable-public-trackers").as_bool().unwrap_or(true),
            bt_piece_selection_strategy: self
                .get("bt-piece-selection-strategy")
                .as_str()
                .unwrap_or("")
                .to_string(),
            bt_endgame_threshold: self.get("bt-endgame-threshold").as_usize() as u32,
            max_retries: self.get("max-tries").as_usize() as u32,
            retry_wait: self.get("retry-wait").as_usize() as u64,
            http_proxy: get_str("http-proxy"),
            all_proxy: get_str("all-proxy"),
            https_proxy: get_str("https-proxy"),
            ftp_proxy: get_str("ftp-proxy"),
            no_proxy: get_str("no-proxy"),
            dht_file_path: get_str("dht-file-path"),
            bt_max_upload_slots: {
                let v = self.get("bt-max-upload-slots").as_usize();
                if v > 0 { Some(v as u32) } else { None }
            },
            bt_optimistic_unchoke_interval: {
                let v = self.get("bt-optimistic-unchoke-interval").as_usize();
                if v > 0 { Some(v as u64) } else { None }
            },
            bt_snubbed_timeout: {
                let v = self.get("bt-snubbed-timeout").as_usize();
                if v > 0 { Some(v as u64) } else { None }
            },
            bt_prioritize_piece: self
                .get("bt-prioritize-piece")
                .as_str()
                .unwrap_or("")
                .to_string(),
            bt_detach_seed_only: self.get("bt-detach-seed-only").as_bool().unwrap_or(false),
            enable_utp: self.get("enable-utp").as_bool().unwrap_or(false),
            utp_listen_port: get_usize("utp-listen-port"),
            header: {
                // C++ aria2 allows repeated `--header NAME: VALUE`; the config
                // parser joins them with newlines. Split on newlines here.
                self.get("header")
                    .as_str()
                    .unwrap_or("")
                    .split('\n')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            },
            user_agent: get_str("user-agent"),
            referer: get_str("referer"),
            file_allocation: get_str("file-allocation"),
            mmap_threshold: get_u64("mmap-threshold"),
            secure_falloc: self.get("secure-falloc").as_bool().unwrap_or(false),
            check_integrity: self.get("check-integrity").as_bool().unwrap_or(false)
                || self.get("hash-check-only").as_bool().unwrap_or(false),
            hash_check_only: self.get("hash-check-only").as_bool().unwrap_or(false),
            // Metalink
            metalink_version: get_str("metalink-version"),
            metalink_language: get_str("metalink-language"),
            metalink_os: get_str("metalink-os"),
            metalink_location: get_str("metalink-location"),
            metalink_preferred_protocol: get_str("metalink-preferred-protocol"),
            select_file: get_str("select-file"),
            piece_length: get_u64("piece-length"),
            metalink_enable_unique_protocol: self
                .get("metalink-enable-unique-protocol")
                .as_bool()
                .unwrap_or(true),
            // FTP
            timeout: get_u64("timeout"),
            connect_timeout: get_u64("connect-timeout"),
            startup_idle_time: get_u64("startup-idle-time"),
            lowest_speed_limit: get_u64("lowest-speed-limit"),
            ftp_pasv: self.get("ftp-pasv").as_bool().unwrap_or(true),
            remote_time: self.get("remote-time").as_bool().unwrap_or(false),
            dry_run: self.get("dry-run").as_bool().unwrap_or(false),
            ftp_reuse_connection: self.get("ftp-reuse-connection").as_bool().unwrap_or(true),
            // Download
            realtime_chunk_checksum: self
                .get("realtime-chunk-checksum")
                .as_bool()
                .unwrap_or(true),
            bt_stop_timeout: get_u64("bt-stop-timeout"),
            // BitTorrent extended
            disable_ipv6: self.get("disable-ipv6").as_bool().unwrap_or(false),
            listen_port: get_str("listen-port"),
            bt_enable_lpd: self.get("bt-enable-lpd").as_bool().unwrap_or(false),
            bt_lpd_interface: get_str("bt-lpd-interface"),
            enable_rpc: self.get("enable-rpc").as_bool().unwrap_or(false),
            pause: self.get("pause").as_bool().unwrap_or(false),
            follow_torrent: None,
            follow_metalink: None,
            // Event hooks
            on_download_start: get_str("on-download-start"),
            on_download_complete: get_str("on-download-complete"),
            on_download_error: get_str("on-download-error"),
            on_download_pause: get_str("on-download-pause"),
            on_download_stop: get_str("on-download-stop"),
            on_bt_download_complete: get_str("on-bt-download-complete"),
            // HTTP authentication
            http_auth_challenge: self.get("http-auth-challenge").as_bool().unwrap_or(false),
            http_user: get_str("http-user"),
            http_passwd: get_str("http-passwd"),
            ftp_user: get_str("ftp-user"),
            ftp_passwd: get_str("ftp-passwd"),
            ssh_host_key_md: get_str("ssh-host-key-md"),
            no_netrc: self.get("no-netrc").as_bool().unwrap_or(false),
            netrc_path: get_str("netrc-path"),
            // Conditional GET
            conditional_get: self.get("conditional-get").as_bool().unwrap_or(false),
        }
    }
}
