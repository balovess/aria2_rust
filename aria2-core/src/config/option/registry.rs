//! Option registry — the central store for all aria2 configuration options.

use std::collections::{HashMap, hash_map::Entry};

use super::types::{OptionCategory, OptionDef, OptionOwner, OptionValue};

/// BitTorrent option names retained as compatibility keys when the feature is
/// disabled. They are registered as unsupported so adapters can reject them
/// explicitly instead of treating them as unknown and skipping value checks.
#[cfg(not(feature = "bittorrent"))]
const UNSUPPORTED_BITTORRENT_OPTIONS: &[(&str, Option<char>)] = &[
    ("bt-detach-seed-only", None),
    ("bt-enable-hook-after-hash-check", None),
    ("bt-enable-lpd", None),
    ("bt-enable-web-seed", None),
    ("bt-endgame-threshold", None),
    ("bt-exclude-tracker", None),
    ("bt-external-ip", None),
    ("bt-force-encryption", Some('X')),
    ("bt-hash-check-seed", None),
    ("bt-incoming-peer-handshake-timeout", None),
    ("bt-keep-alive-interval", None),
    ("bt-load-saved-metadata", None),
    ("bt-lpd-interface", None),
    ("bt-max-open-files", None),
    ("bt-max-peers", Some('B')),
    ("bt-max-upload-slots", None),
    ("bt-metadata-only", None),
    ("bt-min-crypto-level", None),
    ("bt-optimistic-unchoke-interval", None),
    ("bt-peer-blocklist", None),
    ("bt-piece-selection-strategy", None),
    ("bt-prioritize-piece", None),
    ("bt-remove-unselected-file", None),
    ("bt-request-peer-speed-limit", None),
    ("bt-request-timeout", None),
    ("bt-require-crypto", None),
    ("bt-save-metadata", None),
    ("bt-seed-unverified", None),
    ("bt-snubbed-timeout", None),
    ("bt-stop-timeout", None),
    ("bt-timeout", None),
    ("bt-tracker", None),
    ("bt-tracker-connect-timeout", None),
    ("bt-tracker-interval", None),
    ("bt-tracker-source", None),
    ("bt-tracker-source-concurrency", None),
    ("bt-tracker-source-timeout", None),
    ("bt-tracker-stopped-timeout", None),
    ("bt-tracker-timeout", None),
    ("bt-tracker-update-interval", None),
    ("bt-utp-max-connections", None),
    ("dht-entry-point", None),
    ("dht-entry-point-host", None),
    ("dht-entry-point-host6", None),
    ("dht-entry-point-port", None),
    ("dht-entry-point-port6", None),
    ("dht-entry-point6", None),
    ("dht-file-path", None),
    ("dht-file-path6", None),
    ("dht-listen-addr", None),
    ("dht-listen-addr6", None),
    ("dht-listen-port", None),
    ("dht-max-concurrent-lookups", None),
    ("dht-max-items", None),
    ("dht-max-peer-info-hashes", None),
    ("dht-max-peers-per-info-hash", None),
    ("dht-message-timeout", None),
    ("enable-dht", None),
    ("enable-dht6", None),
    ("enable-peer-exchange", None),
    ("enable-public-trackers", None),
    ("enable-utp", None),
    ("follow-torrent", None),
    ("index-out", Some('O')),
    ("listen-port", Some('L')),
    ("lpd-listen-port", None),
    ("on-bt-download-complete", None),
    ("on-bt-download-error", None),
    ("peer-agent", None),
    ("peer-connection-timeout", None),
    ("peer-id-prefix", None),
    ("seed-ratio", Some('g')),
    ("seed-time", Some('G')),
    ("utp-listen-port", None),
];

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
        reg.register_sftp_options();
        #[cfg(feature = "bittorrent")]
        reg.register_bt_options();
        #[cfg(not(feature = "bittorrent"))]
        reg.register_unsupported_bittorrent_options();
        reg.register_rpc_options();
        reg.register_advanced_options();
        reg
    }

    #[cfg(not(feature = "bittorrent"))]
    fn register_unsupported_bittorrent_options(&mut self) {
        for (name, short_name) in UNSUPPORTED_BITTORRENT_OPTIONS {
            self.register(OptionDef {
                name: (*name).into(),
                short_name: *short_name,
                category: OptionCategory::BitTorrent,
                supported: false,
                ..Default::default()
            });
        }
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
            ("bt-max-open-files", OptionOwner::ProcessEngine),
            ("dht-max-peer-info-hashes", OptionOwner::ProcessEngine),
            ("dht-max-peers-per-info-hash", OptionOwner::ProcessEngine),
            ("dht-max-items", OptionOwner::ProcessEngine),
            (
                "bt-incoming-peer-handshake-timeout",
                OptionOwner::ProcessEngine,
            ),
            ("bt-utp-max-connections", OptionOwner::ProcessEngine),
            ("bt-utp-connect-timeout", OptionOwner::ProcessEngine),
            ("bt-utp-idle-timeout", OptionOwner::ProcessEngine),
            ("bt-utp-keepalive-interval", OptionOwner::ProcessEngine),
            ("bt-tracker-source", OptionOwner::ProcessEngine),
            ("bt-tracker-update-interval", OptionOwner::ProcessEngine),
            ("bt-tracker-source-concurrency", OptionOwner::ProcessEngine),
            ("enable-public-trackers", OptionOwner::ProcessEngine),
            ("bt-tracker-source-timeout", OptionOwner::ProcessEngine),
        ];

        for (name, expected_owner) in cases {
            assert_eq!(
                OptionRegistry::owner_for_name(name),
                Some(expected_owner),
                "option '{}' must have the owner of its real execution seam",
                name
            );
            let registry = OptionRegistry::new();
            #[cfg(feature = "bittorrent")]
            assert_eq!(
                registry.get(name).map(|definition| definition.owner()),
                Some(expected_owner),
                "registered option '{}' must preserve its execution owner",
                name
            );
            #[cfg(not(feature = "bittorrent"))]
            if let Some(definition) = registry.get(name) {
                assert_eq!(
                    Some(definition.owner()),
                    Some(expected_owner),
                    "registered option '{}' must preserve its execution owner",
                    name
                );
            }
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
                serde_json::json!(604800),
            );
            options.insert(
                "bt-tracker-source-concurrency".to_string(),
                serde_json::json!(2),
            );
            options.insert(
                "enable-public-trackers".to_string(),
                serde_json::json!(true),
            );
            options.insert("enable-utp".to_string(), serde_json::json!(true));
            options.insert("utp-listen-port".to_string(), serde_json::json!(6882));
            options.insert(
                "dht-max-peer-info-hashes".to_string(),
                serde_json::json!(1200),
            );
            options.insert(
                "dht-max-peers-per-info-hash".to_string(),
                serde_json::json!(37),
            );
        }

        let projected = registry.project_defined_global_options_for_rpc(&options);

        assert_eq!(projected.get("dns-timeout"), Some(&serde_json::json!(30)));
        assert!(!projected.contains_key("rpc-secret"));
        assert!(!projected.contains_key("not-a-registered-option"));
        #[cfg(feature = "bittorrent")]
        {
            assert!(!projected.contains_key("enable-utp"));
            assert!(!projected.contains_key("utp-listen-port"));
            assert_eq!(
                projected.get("bt-tracker-source"),
                Some(&serde_json::json!("https://example.test/trackers.txt"))
            );
            assert_eq!(
                projected.get("bt-tracker-update-interval"),
                Some(&serde_json::json!(604800))
            );
            assert_eq!(
                projected.get("bt-tracker-source-concurrency"),
                Some(&serde_json::json!(2))
            );
            assert_eq!(
                projected.get("enable-public-trackers"),
                Some(&serde_json::json!(true))
            );
            assert_eq!(
                projected.get("dht-max-peer-info-hashes"),
                Some(&serde_json::json!(1200))
            );
            assert_eq!(
                projected.get("dht-max-peers-per-info-hash"),
                Some(&serde_json::json!(37))
            );
        }
    }
}
