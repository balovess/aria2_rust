//! Option registry — the central store for all aria2 configuration options.

use std::collections::{HashMap, hash_map::Entry};

use super::types::{OptionCategory, OptionDef, OptionValue};

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
        let name = def.name().to_owned();
        match self.options.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(def);
            }
            Entry::Occupied(entry) => {
                panic!("duplicate configuration option '{}'", entry.key());
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&OptionDef> {
        self.options.get(Self::canonical_name(name))
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
    fn canonical_name(name: &str) -> &str {
        match name {
            "max-retries" => "max-tries",
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

    use super::OptionRegistry;

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
