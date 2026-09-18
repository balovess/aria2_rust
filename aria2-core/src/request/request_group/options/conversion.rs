use super::DownloadOptions;
use crate::config::OptionRegistry;

/// Convert a JSON-RPC option value to the string representation used by
/// aria2's option handlers.
///
/// JSON-RPC/XML-RPC callers normally provide strings. Numeric and boolean
/// values are accepted by the Rust API as an extension, while arrays are
/// joined for cumulative options such as `header`.
pub fn option_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Array(values) => values
            .iter()
            .map(option_value_to_string)
            .collect::<Option<Vec<_>>>()
            .map(|values| values.join("\n")),
        serde_json::Value::Null | serde_json::Value::Object(_) => None,
    }
}

impl DownloadOptions {
    /// Build per-download options from typed configuration values.
    ///
    /// Configuration managers use [`OptionValue`](crate::config::OptionValue)
    /// while session files and RPC option maps use strings. Converting both
    /// through this seam keeps the download engine independent of the source
    /// of the options and gives every caller the same default handling.
    pub fn from_option_values(
        options: &std::collections::HashMap<String, crate::config::OptionValue>,
    ) -> Self {
        let string_options = options
            .iter()
            .filter(|(_, value)| !value.is_none())
            .map(|(key, value)| (key.clone(), value.to_string()))
            .collect();
        Self::from_option_strings(&string_options)
    }

    /// Build per-download options from an RPC option map.
    ///
    /// aria2's JSON-RPC and XML-RPC interfaces use strings for option values;
    /// arrays are accepted for cumulative options such as `header` and are
    /// joined with newlines before entering the shared string parser. Numeric
    /// and boolean JSON values are accepted as a harmless extension for
    /// existing Rust clients, then canonicalized to the same string form.
    pub fn from_rpc_options(
        options: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        Self::try_from_rpc_options(options).unwrap_or_default()
    }

    /// Fallibly build per-download options from an RPC option map.
    ///
    /// The registry is the validation seam for task creation. Unknown option
    /// names remain ignored, matching aria2's RPC option gatherer, while
    /// known options must pass the same type, range, and enum checks as the
    /// configuration path. The infallible [`Self::from_rpc_options`] helper is
    /// retained for compatibility with older in-process callers; external
    /// adapters must use this method so invalid values cannot become defaults.
    pub fn try_from_rpc_options(
        options: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<Self, String> {
        let registry = crate::config::OptionRegistry::new();
        let mut string_options = std::collections::HashMap::with_capacity(options.len());
        for (key, value) in options {
            if registry.get(key).is_none() {
                continue;
            }
            registry
                .parse_rpc_value(key, value)
                .map_err(|error| format!("Option '{}': {}", key, error))?;
            let value = option_value_to_string(value)
                .ok_or_else(|| format!("Option '{}' must be a string", key))?;
            string_options.insert(OptionRegistry::canonical_name(key).to_string(), value);
        }
        Ok(Self::from_option_strings(&string_options))
    }
}
