//! Runtime option updates and rate limiter management.
//!
//! Implements `RequestGroup::update_option()` for dynamically changing
//! download options at runtime (e.g. via `aria2.changeOption`), and
//! the `set_rate_limiter` / `set_download_context` methods.

use std::sync::Arc;

use crate::rate_limiter::RateLimiter;
use crate::util::rwlock_ext::RwLockRecover;

impl super::RequestGroup {
    // ── Basic Accessors ─────────────────────────────────────────────────

    /// Return the group ID.
    pub fn gid(&self) -> super::GroupId {
        self.gid
    }

    /// Return the initial URI list.
    ///
    /// Note: This returns the *initial* URIs provided when the group was
    /// created. For the current remaining/spent URI state, use
    /// `get_remaining_uris()` / `get_spent_uris()` which delegate to
    /// `FileEntry` via `DownloadContext`.
    pub fn uris(&self) -> &[String] {
        &self.uris
    }

    /// Set a per-group output filename, used by Metalink entries.
    pub fn set_output_name(&self, name: impl Into<String>) {
        *self.output_name.recover_mut() = Some(name.into());
    }

    /// Return the per-group output filename, if configured.
    pub fn output_name(&self) -> Option<String> {
        self.output_name.recover().clone()
    }

    /// Return a reference to the download options.
    pub fn options(&self) -> &super::DownloadOptions {
        &self.options
    }

    /// Cheap clone of the options `Arc` — O(1) refcount bump instead of
    /// deep-cloning all `Vec<String>` fields.
    pub fn options_arc(&self) -> Arc<super::DownloadOptions> {
        Arc::clone(&self.options)
    }

    // ── Rate Limiter ────────────────────────────────────────────────────

    /// Store a handle to the download's `RateLimiter` so that runtime option
    /// updates (e.g. via `aria2.changeOption`) can dynamically adjust the rate.
    pub fn set_rate_limiter(&self, limiter: RateLimiter) {
        *self.rate_limiter.recover_mut() = Some(limiter);
    }

    /// Store options that take effect when the next command generation starts.
    pub fn set_pending_options(
        &self,
        changes: std::collections::HashMap<String, serde_json::Value>,
    ) {
        if let Ok(mut pending) = self.pending_options.write() {
            pending.extend(changes);
        }
    }

    /// Apply and clear options deferred by `changeOption`.
    pub fn apply_pending_options(&mut self) {
        let changes = self
            .pending_options
            .write()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for (key, value) in changes {
            self.update_option(&key, value);
        }
    }

    pub fn pending_options(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.pending_options
            .read()
            .map(|pending| pending.clone())
            .unwrap_or_default()
    }

    // ── Runtime Option Updates ──────────────────────────────────────────

    /// Update a single runtime-changeable option by key (using aria2's
    /// kebab-case option names, e.g. `"max-download-limit"`).
    ///
    /// Returns `true` if the option was recognized and updated, `false` if the
    /// key is not a runtime-changeable option.
    ///
    /// For `max-download-limit` / `max-upload-limit`, the stored
    /// `RateLimiter` (if any) is also updated so the change takes effect
    /// immediately on the live download.
    pub fn update_option(&mut self, key: &str, value: serde_json::Value) -> bool {
        let opts = Arc::make_mut(&mut self.options);
        match key {
            "split" => {
                if let Some(v) = value.as_u64() {
                    opts.split = Some(v as u16);
                    tracing::warn!(
                        new_split = v,
                        "split changed but will take effect on download restart/retry, \
                         not mid-download (current segments unchanged)"
                    );
                }
                true
            }
            "max-download-limit" => {
                let rate = value.as_u64();
                opts.max_download_limit = rate;
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_download_rate(rate);
                }
                true
            }
            "max-upload-limit" => {
                let rate = value.as_u64();
                opts.max_upload_limit = rate;
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_upload_rate(rate);
                }
                true
            }
            "max-tries" | "max-retries" => {
                if let Some(v) = value.as_u64() {
                    opts.max_retries = v as u32;
                }
                true
            }
            "retry-wait" => {
                if let Some(v) = value.as_u64() {
                    opts.retry_wait = v;
                }
                true
            }
            "header" => {
                match &value {
                    serde_json::Value::Array(arr) => {
                        opts.header = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect();
                    }
                    serde_json::Value::String(s) => {
                        opts.header = s
                            .split('\n')
                            .map(|l| l.trim().to_string())
                            .filter(|l| !l.is_empty())
                            .collect();
                    }
                    _ => {}
                }
                true
            }
            "user-agent" => {
                opts.user_agent = value.as_str().map(|s| s.to_string());
                true
            }
            "referer" => {
                opts.referer = value.as_str().map(|s| s.to_string());
                true
            }
            "max-connection-per-server" => {
                if let Some(v) = value.as_u64() {
                    opts.max_connection_per_server = Some(v as u16);
                }
                true
            }
            "bt-max-upload-slots" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_max_upload_slots = Some(v as u32);
                }
                true
            }
            "bt-snubbed-timeout" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_snubbed_timeout = Some(v);
                }
                true
            }
            "bt-optimistic-unchoke-interval" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_optimistic_unchoke_interval = Some(v);
                }
                true
            }
            "bt-endgame-threshold" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_endgame_threshold = v as u32;
                }
                true
            }
            "seed-time" => {
                if let Some(v) = value.as_f64() {
                    opts.seed_time = Some(v);
                }
                true
            }
            "seed-ratio" => {
                if let Some(v) = value.as_f64() {
                    opts.seed_ratio = Some(v);
                }
                true
            }
            "bt-detach-seed-only" => {
                if let Some(v) = value.as_bool() {
                    opts.bt_detach_seed_only = v;
                }
                true
            }
            "dir" => {
                opts.dir = value.as_str().map(|s| s.to_string());
                true
            }
            "out" => {
                opts.out = value.as_str().map(|s| s.to_string());
                true
            }
            "file-allocation" => {
                if let Some(s) = value.as_str() {
                    opts.file_allocation = Some(s.to_string());
                }
                true
            }
            "mmap-threshold" => {
                opts.mmap_threshold = value.as_u64();
                true
            }
            "secure-falloc" => {
                opts.secure_falloc = value.as_bool().unwrap_or(false);
                true
            }
            "checksum" => {
                if let Some(s) = value.as_str()
                    && let Some((algo, hash)) = s.split_once('=')
                {
                    opts.checksum = Some((algo.to_string(), hash.to_string()));
                }
                true
            }
            "cookie-file" => {
                opts.cookie_file = value.as_str().map(|s| s.to_string());
                true
            }
            "cookies" => {
                opts.cookies = value.as_str().map(|s| s.to_string());
                true
            }
            "bt-force-encryption" | "bt-force-encrypt" => {
                opts.bt_force_encrypt = value.as_bool().unwrap_or(false);
                true
            }
            "bt-require-crypto" => {
                opts.bt_require_crypto = value.as_bool().unwrap_or(false);
                true
            }
            "enable-dht" => {
                opts.enable_dht = value.as_bool().unwrap_or(true);
                true
            }
            "dht-listen-port" => {
                opts.dht_listen_port = value.as_u64().map(|v| v as u16);
                true
            }
            "dht-entry-point" => {
                match &value {
                    serde_json::Value::String(s) => {
                        opts.dht_entry_point = Some(vec![s.to_string()]);
                    }
                    serde_json::Value::Array(arr) => {
                        opts.dht_entry_point = Some(
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect(),
                        );
                    }
                    _ => {}
                }
                true
            }
            "enable-public-trackers" => {
                opts.enable_public_trackers = value.as_bool().unwrap_or(true);
                true
            }
            "bt-piece-selection-strategy" => {
                if let Some(s) = value.as_str() {
                    opts.bt_piece_selection_strategy = s.to_string();
                }
                true
            }
            "bt-prioritize-piece" => {
                if let Some(s) = value.as_str() {
                    opts.bt_prioritize_piece = s.to_string();
                }
                true
            }
            "enable-utp" => {
                opts.enable_utp = value.as_bool().unwrap_or(false);
                true
            }
            "utp-listen-port" => {
                opts.utp_listen_port = value.as_u64().map(|v| v as u16);
                true
            }
            "dht-file-path" => {
                opts.dht_file_path = value.as_str().map(|s| s.to_string());
                true
            }
            "http-proxy" => {
                opts.http_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "all-proxy" => {
                opts.all_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "https-proxy" => {
                opts.https_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "ftp-proxy" => {
                opts.ftp_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "no-proxy" => {
                opts.no_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            _ => false,
        }
    }
}
