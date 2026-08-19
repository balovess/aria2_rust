//! Option registry — the central store for all aria2 configuration options.

use std::collections::{HashMap, hash_map::Entry};

use super::types::{OptionCategory, OptionDef, OptionOwner, OptionValue};

/// Registry of all known configuration options.
#[derive(Clone)]
pub struct OptionRegistry {
    pub(super) options: HashMap<String, OptionDef>,
}

impl OptionRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            options: HashMap::new(),
        };
        reg.register_general_options();
        reg.register_http_ftp_options();
        #[cfg(feature = "bittorrent")]
        reg.register_bt_options();
        reg.register_rpc_options();
        reg.register_advanced_options();
        reg
    }

    pub fn register(&mut self, def: OptionDef) {
        let mut def = def;
        let name = def.name().to_owned();
        let canonical_name = Self::canonical_name(&name);
        if canonical_name != name {
            panic!(
                "configuration option '{}' is an alias; register canonical option '{}' instead",
                name, canonical_name
            );
        }
        if self.options.contains_key(&name) {
            panic!("duplicate configuration option '{}'", name);
        }
        if let Some(short_name) = def.short_name()
            && let Some(existing) = self
                .options
                .values()
                .find(|existing| existing.short_name() == Some(short_name))
        {
            panic!(
                "duplicate short option '-{}' for '{}' and '{}'",
                short_name,
                existing.name(),
                name
            );
        }
        if let Some(expected_owner) = Self::owner_for_name(&name) {
            if def.owner() != OptionOwner::Unassigned && def.owner() != expected_owner {
                panic!(
                    "configuration option '{}' has owner {:?}, expected {:?}",
                    name,
                    def.owner(),
                    expected_owner
                );
            }
            def.owner = expected_owner;
        } else if def.owner() == OptionOwner::Unassigned {
            panic!(
                "configuration option '{}' has no explicit production owner",
                name
            );
        }
        match self.options.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(def);
            }
            Entry::Occupied(entry) => {
                panic!("duplicate configuration option '{}'", entry.key());
            }
        }
    }

    /// Override a registered option's built-in default value.
    ///
    /// Embedding applications can use this for product-owned defaults while
    /// keeping the core library's standalone defaults unchanged.
    pub fn set_default_value(
        &mut self,
        name: &str,
        value: super::types::OptionValue,
    ) -> Result<(), String> {
        let definition = self
            .options
            .get_mut(Self::canonical_name(name))
            .ok_or_else(|| format!("unknown option '{}'", name))?;
        definition.default_value = value;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&OptionDef> {
        self.options.get(Self::canonical_name(name))
    }

    /// Return the option bound to a short command-line spelling.
    ///
    /// Short names are validated for uniqueness during registration, so every
    /// lookup has at most one result and CLI parsers do not need to reimplement
    /// the registry traversal.
    pub fn get_by_short_name(&self, short_name: char) -> Option<&OptionDef> {
        self.options
            .values()
            .find(|definition| definition.short_name() == Some(short_name))
    }

    /// Parse a value received from a JSON/XML-RPC or FFI adapter using the
    /// same definition that validates CLI and configuration-file values.
    ///
    /// Adapters may accept native JSON numbers and booleans as a convenience,
    /// but the option definition remains the source of truth for ranges,
    /// booleans, sizes, and enum choices. Arrays are represented by the same
    /// newline-separated wire form used by aria2 for cumulative options.
    pub fn parse_rpc_value(
        &self,
        name: &str,
        value: &serde_json::Value,
    ) -> Result<OptionValue, String> {
        let definition = self
            .get(name)
            .ok_or_else(|| format!("unknown option '{}'", name))?;
        if value.is_array() && definition.cumulative_delimiter.is_none() {
            return Err("option value must be a string".to_string());
        }
        let raw = rpc_value_to_string(value)?;
        definition.parse_value(&raw)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn all(&self) -> &HashMap<String, OptionDef> {
        &self.options
    }

    /// Return the currently defined global options that may appear in
    /// `aria2.getGlobalOption`.
    ///
    /// C++ iterates its option table and emits values only when an
    /// `OptionHandler` exists and the preference is defined, with
    /// `rpc-secret` as the explicit exception. The registry owns the
    /// equivalent metadata here so RPC adapters do not grow their own
    /// string-based allowlists. Hidden and deprecated original options remain
    /// visible when defined; Rust-only extensions are excluded.
    pub fn project_defined_global_options_for_rpc(
        &self,
        options: &HashMap<String, serde_json::Value>,
    ) -> HashMap<String, serde_json::Value> {
        self.options
            .iter()
            .filter(|(_, definition)| definition.is_exposed_in_aria2_rpc())
            .filter_map(|(name, _)| {
                options
                    .get(name)
                    .cloned()
                    .map(|value| (name.clone(), value))
            })
            .collect()
    }

    pub fn count(&self) -> usize {
        self.options.len()
    }

    pub fn by_category(&self, cat: OptionCategory) -> Vec<&OptionDef> {
        self.options
            .values()
            .filter(|d| d.get_category() == cat)
            .collect()
    }
}

impl Default for OptionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OptionRegistry {
    /// Map internal option spellings to the original aria2 option names.
    ///
    /// `DownloadOptions` historically exposed `max-retries`, while aria2's
    /// public CLI/RPC option is `max-tries`. Keeping the alias here lets every
    /// adapter validate both spellings through the same definition without
    /// duplicating parser logic.
    pub fn canonical_name(name: &str) -> &str {
        match name {
            "max-retries" => "max-tries",
            "enable-lpd" => "bt-enable-lpd",
            "dht-message-path" => "dht-file-path",
            "server-stat-file" => "server-stat-of",
            "max-downloads" => "max-concurrent-downloads",
            _ => name,
        }
    }

    /// Return the explicit production owner for a canonical built-in option.
    ///
    /// This list is deliberately exhaustive rather than derived from
    /// [`OptionCategory`]. Adding a built-in option without adding it here
    /// makes registry construction fail immediately.
    pub fn owner_for_name(name: &str) -> Option<OptionOwner> {
        let name = Self::canonical_name(name);
        match name {
            // Per-download HTTP/FTP and shared download settings.
            "all-proxy"
            | "http-proxy"
            | "https-proxy"
            | "ftp-proxy"
            | "no-proxy"
            | "all-proxy-user"
            | "all-proxy-passwd"
            | "http-proxy-user"
            | "http-proxy-passwd"
            | "https-proxy-user"
            | "https-proxy-passwd"
            | "ftp-proxy-user"
            | "ftp-proxy-passwd"
            | "proxy-method"
            | "user-agent"
            | "referer"
            | "header"
            | "load-cookies"
            | "save-cookies"
            | "connect-timeout"
            | "timeout"
            | "max-tries"
            | "retry-wait"
            | "split"
            | "min-split-size"
            | "max-connection-per-server"
            | "check-certificate"
            | "ca-certificate"
            | "allow-overwrite"
            | "auto-file-renaming"
            | "continue"
            | "remote-time"
            | "enable-http-keep-alive"
            | "enable-http-pipelining"
            | "http-accept-gzip"
            | "http-auth-challenge"
            | "http-no-cache"
            | "content-disposition-default-utf8"
            | "use-head"
            | "no-want-digest-header"
            | "http-user"
            | "http-passwd"
            | "certificate"
            | "private-key"
            | "min-tls-version"
            | "max-http-pipelining"
            | "ftp-user"
            | "ftp-passwd"
            | "ftp-pasv"
            | "ftp-reuse-connection"
            | "ftp-type"
            | "ssh-host-key-md"
            | "dir"
            | "out"
            | "allow-piece-length-change"
            | "always-resume"
            | "check-integrity"
            | "conditional-get"
            | "disable-ipv6"
            | "hash-check-only"
            | "parameterized-uri"
            | "pause"
            | "remove-control-file"
            | "reuse-uri"
            | "save-not-found"
            | "force-sequential"
            | "no-netrc"
            | "realtime-chunk-checksum"
            | "lowest-speed-limit"
            | "max-file-not-found"
            | "no-file-allocation-limit"
            | "uri-selector"
            | "stream-piece-selector"
            | "checksum"
            | "file-allocation"
            | "secure-falloc"
            | "mmap-threshold"
            | "enable-mmap"
            | "max-mmap-limit"
            | "max-download-limit"
            | "max-upload-limit"
            | "piece-length"
            | "disk-cache"
            | "max-resume-failure-tries"
            | "show-files"
            | "metalink-version"
            | "metalink-language"
            | "metalink-os"
            | "metalink-location"
            | "metalink-preferred-protocol"
            | "metalink-enable-unique-protocol"
            | "metalink-base-uri"
            | "follow-metalink"
            | "seed-time"
            | "seed-ratio"
            | "bt-max-peers"
            | "bt-request-peer-speed-limit"
            | "bt-max-open-files"
            | "bt-seed-unverified"
            | "bt-save-metadata"
            | "bt-force-encryption"
            | "bt-min-crypto-level"
            | "bt-detach-seed-only"
            | "bt-enable-lpd"
            | "bt-enable-web-seed"
            | "enable-dht"
            | "dht-listen-port"
            | "dht-entry-point"
            | "dht-file-path"
            | "enable-peer-exchange"
            | "follow-torrent"
            | "on-bt-download-complete"
            | "on-bt-download-error"
            | "bt-prioritize-piece"
            | "bt-enable-hook-after-hash-check"
            | "bt-exclude-tracker"
            | "bt-external-ip"
            | "bt-hash-check-seed"
            | "bt-load-saved-metadata"
            | "bt-metadata-only"
            | "bt-remove-unselected-file"
            | "bt-require-crypto"
            | "bt-stop-timeout"
            | "bt-tracker"
            | "bt-tracker-source"
            | "bt-tracker-update-interval"
            | "enable-public-trackers"
            | "bt-tracker-connect-timeout"
            | "bt-tracker-interval"
            | "bt-tracker-timeout"
            | "dht-message-timeout"
            | "enable-dht6"
            | "dht-listen-addr6"
            | "peer-id-prefix"
            | "peer-agent"
            | "select-file"
            | "index-out"
            | "bt-peer-blocklist"
            | "enable-utp"
            | "utp-listen-port"
            | "bt-keep-alive-interval"
            | "bt-timeout"
            | "bt-request-timeout"
            | "peer-connection-timeout"
            | "pause-metadata"
            | "dht-entry-point-host"
            | "dht-entry-point-port"
            | "dht-entry-point6"
            | "dht-entry-point-host6"
            | "dht-entry-point-port6"
            | "dht-file-path6"
            | "dht-listen-addr"
            | "listen-port" => Some(OptionOwner::DownloadTask),

            // Process-wide scheduler, resolver, socket, persistence, and
            // statistics settings.
            "async-dns-server"
            | "dns-timeout"
            | "event-poll"
            | "interface"
            | "multiple-interface"
            | "server-stat-timeout"
            | "server-stat-if"
            | "server-stat-of"
            | "max-concurrent-downloads"
            | "max-overall-download-limit"
            | "max-overall-upload-limit"
            | "stop"
            | "force-save"
            | "save-server-stat-interval"
            | "socket-recv-buffer-size"
            | "dscp"
            | "optimize-concurrent-downloads"
            | "optimize-concurrent-downloads-coeffA"
            | "optimize-concurrent-downloads-coeffB"
            | "rlimit-nofile"
            | "select-least-used-host"
            | "stop-with-process"
            | "log-max-size"
            | "log-max-files"
            | "lpd-listen-port"
            | "bt-lpd-interface" => Some(OptionOwner::ProcessEngine),

            "async-dns" | "enable-async-dns6" | "startup-idle-time" => {
                Some(OptionOwner::DownloadTask)
            }

            // CLI, session, terminal, and process lifecycle settings.
            "conf-path"
            | "no-conf"
            | "input-file"
            | "save-session"
            | "save-session-interval"
            | "auto-save-interval"
            | "daemon"
            | "pid-file"
            | "gid"
            | "netrc-path"
            | "deferred-input"
            | "max-download-result"
            | "on-download-start"
            | "on-download-stop"
            | "on-download-pause"
            | "on-download-complete"
            | "on-download-error"
            | "torrent-file"
            | "metalink-file"
            | "enable-color"
            | "quiet"
            | "dry-run"
            | "download-result"
            | "human-readable"
            | "keep-unfinished-download-result"
            | "truncate-console-readout"
            | "show-console-readout"
            | "stderr"
            | "log"
            | "log-level"
            | "console-log-level"
            | "log-backup-count"
            | "summary-interval" => Some(OptionOwner::Application),

            // RPC listener and authentication settings.
            "enable-rpc"
            | "rpc-listen-all"
            | "rpc-listen-port"
            | "rpc-listen-address"
            | "rpc-secret"
            | "rpc-user"
            | "rpc-passwd"
            | "rpc-allow-origin"
            | "rpc-cors-domain"
            | "rpc-secure"
            | "rpc-certificate"
            | "rpc-private-key"
            | "rpc-allow-origin-all"
            | "rpc-max-request-size"
            | "rpc-save-upload-metadata" => Some(OptionOwner::RpcServer),

            _ => None,
        }
    }
}

fn rpc_value_to_string(value: &serde_json::Value) -> Result<String, String> {
    match value {
        serde_json::Value::String(value) => Ok(value.clone()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Array(values) => values
            .iter()
            .map(rpc_value_to_string)
            .collect::<Result<Vec<_>, _>>()
            .map(|values| values.join("\n")),
        serde_json::Value::Null => Err("option value must not be null".to_string()),
        serde_json::Value::Object(_) => Err("option value must be scalar or array".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{OptionOwner, OptionRegistry};

    #[test]
    fn compatibility_aliases_resolve_to_one_registered_definition() {
        let registry = OptionRegistry::new();
        let aliases = [
            ("max-retries", "max-tries"),
            ("enable-lpd", "bt-enable-lpd"),
            ("dht-message-path", "dht-file-path"),
            ("server-stat-file", "server-stat-of"),
            ("max-downloads", "max-concurrent-downloads"),
        ];

        for (alias, canonical) in aliases {
            assert_eq!(OptionRegistry::canonical_name(alias), canonical);
            assert!(
                !registry.all().contains_key(alias),
                "alias '{}' must not create a second registry definition",
                alias
            );
            if let Some(definition) = registry.get(canonical) {
                assert_eq!(registry.get(alias).map(|def| def.name()), Some(canonical));
                assert_eq!(definition.name(), canonical);
            }
        }
    }

    #[test]
    fn built_in_options_have_explicit_owner_mappings() {
        let registry = OptionRegistry::new();
        for (name, definition) in registry.all() {
            assert_eq!(
                OptionRegistry::owner_for_name(name),
                Some(definition.owner()),
                "registered option '{}' must have an explicit canonical owner",
                name
            );
            assert_ne!(definition.owner(), OptionOwner::Unassigned);
        }
    }

    #[test]
    fn shared_runtime_options_have_one_execution_owner() {
        let cases = [
            ("async-dns", OptionOwner::DownloadTask),
            ("enable-async-dns6", OptionOwner::DownloadTask),
            ("startup-idle-time", OptionOwner::DownloadTask),
            ("async-dns-server", OptionOwner::ProcessEngine),
            ("dns-timeout", OptionOwner::ProcessEngine),
        ];

        for (name, expected_owner) in cases {
            assert_eq!(
                OptionRegistry::owner_for_name(name),
                Some(expected_owner),
                "option '{}' must have the owner of its real execution seam",
                name
            );
            assert_eq!(
                OptionRegistry::new()
                    .get(name)
                    .map(|definition| definition.owner()),
                Some(expected_owner),
                "registered option '{}' must preserve its execution owner",
                name
            );
        }
    }

    #[test]
    fn global_rpc_projection_keeps_hidden_original_options_outside_help_and_extensions() {
        let registry = OptionRegistry::new();
        #[allow(unused_mut)]
        let mut options = HashMap::from([
            ("dns-timeout".to_string(), serde_json::json!(30)),
            ("rpc-secret".to_string(), serde_json::json!("do-not-return")),
            (
                "not-a-registered-option".to_string(),
                serde_json::json!("private"),
            ),
        ]);
        #[cfg(feature = "bittorrent")]
        {
            options.insert(
                "bt-tracker-source".to_string(),
                serde_json::json!("https://example.test/trackers.txt"),
            );
            options.insert(
                "bt-tracker-update-interval".to_string(),
                serde_json::json!(86400),
            );
            options.insert(
                "enable-public-trackers".to_string(),
                serde_json::json!(true),
            );
            options.insert("enable-utp".to_string(), serde_json::json!(true));
            options.insert("utp-listen-port".to_string(), serde_json::json!(6882));
        }

        let projected = registry.project_defined_global_options_for_rpc(&options);

        assert_eq!(projected.get("dns-timeout"), Some(&serde_json::json!(30)));
        assert!(!projected.contains_key("rpc-secret"));
        assert!(!projected.contains_key("not-a-registered-option"));
        #[cfg(feature = "bittorrent")]
        {
            assert!(!projected.contains_key("enable-utp"));
            assert!(!projected.contains_key("utp-listen-port"));
            assert!(!projected.contains_key("bt-tracker-source"));
            assert!(!projected.contains_key("bt-tracker-update-interval"));
            assert!(!projected.contains_key("enable-public-trackers"));
        }
    }
}
