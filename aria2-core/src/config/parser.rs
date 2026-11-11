use std::collections::HashMap;
use std::fmt;

use super::option::{OptionRegistry, OptionType, OptionValue};

#[derive(Debug, Clone)]
pub enum ConfigSource {
    Defaults,
    Environment,
    ConfigFile,
    CommandLine,
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Defaults => write!(f, "defaults"),
            Self::Environment => write!(f, "environment"),
            Self::ConfigFile => write!(f, "config-file"),
            Self::CommandLine => write!(f, "command-line"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigError {
    pub source: ConfigSource,
    pub option: String,
    pub message: String,
}

/// Source location for an error found while parsing a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrorContext {
    pub line: usize,
    pub content: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.source, self.option, self.message)
    }
}

pub struct ConfigParser {
    options: HashMap<String, OptionValue>,
    option_sources: HashMap<String, ConfigSource>,
    sources: Vec<ConfigSource>,
    errors: Vec<ConfigError>,
    error_contexts: HashMap<usize, ConfigErrorContext>,
    registry: OptionRegistry,
}

impl ConfigParser {
    pub fn new() -> Self {
        Self {
            options: HashMap::new(),
            option_sources: HashMap::new(),
            sources: Vec::new(),
            errors: Vec::new(),
            error_contexts: HashMap::new(),
            registry: OptionRegistry::new(),
        }
    }

    pub fn with_registry(registry: OptionRegistry) -> Self {
        Self {
            registry,
            ..Self::new()
        }
    }

    pub fn set(&mut self, name: impl Into<String>, value: OptionValue) {
        let original_key = name.into();
        let key = OptionRegistry::canonical_name(&original_key).to_string();
        if let Some(def) = self.registry.get(&key) {
            match def.parse_value(&value.to_string()) {
                Ok(v) => {
                    self.options.insert(key.clone(), v);
                    self.option_sources.insert(key, ConfigSource::CommandLine);
                }
                Err(e) => self.errors.push(ConfigError {
                    source: ConfigSource::CommandLine,
                    option: original_key,
                    message: e,
                }),
            }
        } else {
            self.options.insert(key.clone(), value);
            self.option_sources.insert(key, ConfigSource::CommandLine);
        }
    }

    pub fn set_raw(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.set_raw_from(name, value, ConfigSource::CommandLine);
    }

    fn set_raw_from(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        source: ConfigSource,
    ) {
        let original_key = name.into();
        let key = OptionRegistry::canonical_name(&original_key).to_string();
        let val_str = value.into();
        if let Some(def) = self.registry.get(&key) {
            // Check if this option supports cumulative (appending) values
            if def.cumulative_delimiter.is_some() && self.options.contains_key(&key) {
                match def.parse_value(&val_str) {
                    Ok(new_parsed) => {
                        let existing = self
                            .options
                            .get(&key)
                            .cloned()
                            .unwrap_or(new_parsed.clone());
                        match (existing, new_parsed) {
                            (
                                OptionValue::List(mut existing_list),
                                OptionValue::List(new_items),
                            ) => {
                                existing_list.extend(new_items);
                                self.options
                                    .insert(key.clone(), OptionValue::List(existing_list));
                                self.option_sources.insert(key, source.clone());
                            }
                            (OptionValue::Str(existing_str), OptionValue::Str(new_str)) => {
                                let delim = def.cumulative_delimiter.unwrap_or("");
                                let combined = existing_str + delim + &new_str;
                                self.options.insert(key.clone(), OptionValue::Str(combined));
                                self.option_sources.insert(key, source.clone());
                            }
                            (_, new) => {
                                self.options.insert(key.clone(), new);
                                self.option_sources.insert(key, source.clone());
                            }
                        }
                    }
                    Err(e) => self.errors.push(ConfigError {
                        source: source.clone(),
                        option: original_key.clone(),
                        message: e,
                    }),
                }
            } else {
                match def.parse_value(&val_str) {
                    Ok(v) => {
                        self.options.insert(key.clone(), v);
                        self.option_sources.insert(key, source.clone());
                    }
                    Err(e) => self.errors.push(ConfigError {
                        source: source.clone(),
                        option: original_key.clone(),
                        message: e,
                    }),
                }
            }
        } else {
            self.options
                .insert(original_key.clone(), OptionValue::Str(val_str));
            self.option_sources.insert(original_key, source);
        }
    }

    pub fn get(&self, name: &str) -> Option<&OptionValue> {
        self.options.get(OptionRegistry::canonical_name(name))
    }
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| v.as_str())
    }
    pub fn get_i64(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(|v| v.as_i64())
    }
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        self.get(name).and_then(|v| v.as_bool())
    }
    pub fn contains(&self, name: &str) -> bool {
        self.options
            .contains_key(OptionRegistry::canonical_name(name))
    }

    pub fn option_source(&self, name: &str) -> Option<&ConfigSource> {
        self.option_sources
            .get(OptionRegistry::canonical_name(name))
    }

    pub fn parse_cli_args(&mut self, args: &[&str]) {
        self.sources.push(ConfigSource::CommandLine);
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(opt_name) = arg.strip_prefix("--") {
                if let Some((name, value)) = opt_name.split_once('=') {
                    if name.starts_with("no-") && name.len() > 3 && !self.registry.contains(name) {
                        // A `--no-foo` spelling is a boolean negation only
                        // when `no-foo` is not itself a registered option.
                        self.set(name[3..].to_string(), OptionValue::Bool(false));
                    } else {
                        self.set_raw(name, value);
                    }
                } else if opt_name.starts_with("no-") && opt_name.len() > 3 {
                    // Check if `no-<name>` exists as a full option name first (e.g., "no-conf")
                    if self.registry.contains(opt_name) {
                        self.set(opt_name, OptionValue::Bool(true));
                    } else {
                        // `no-X` means set option X to false (e.g., "no-check-certificate")
                        let real_name = &opt_name[3..];
                        self.set(real_name, OptionValue::Bool(false));
                    }
                } else if let Some(def) = self.registry.get(opt_name) {
                    if def.opt_type() == OptionType::Boolean {
                        self.set(opt_name, OptionValue::Bool(true));
                    } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                        self.set_raw(opt_name, args[i + 1]);
                        i += 1;
                    } else {
                        self.errors.push(ConfigError {
                            source: ConfigSource::CommandLine,
                            option: opt_name.to_string(),
                            message: "option requires a value".to_string(),
                        });
                    }
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    self.set_raw(opt_name, args[i + 1]);
                    i += 1;
                } else {
                    self.set_raw(opt_name, "");
                }
            } else if arg.starts_with('-') && arg.len() == 2 {
                let c = arg.chars().nth(1).unwrap();
                let opt_name = self
                    .registry
                    .get_by_short_name(c)
                    .map(|def| def.name().to_string());
                if let Some(name) = opt_name {
                    if self
                        .registry
                        .get(&name)
                        .is_some_and(|def| def.opt_type() != OptionType::Boolean)
                        && i + 1 < args.len()
                        && !args[i + 1].starts_with('-')
                    {
                        self.set_raw(&name, args[i + 1]);
                        i += 1;
                    } else if self
                        .registry
                        .get(&name)
                        .is_some_and(|def| def.opt_type() == OptionType::Boolean)
                    {
                        self.set(&name, OptionValue::Bool(true));
                    } else {
                        self.errors.push(ConfigError {
                            source: ConfigSource::CommandLine,
                            option: name,
                            message: "option requires a value".to_string(),
                        });
                    }
                }
            } else if let Some(rest) = arg.strip_prefix('@') {
                self.parse_file(rest);
            } else {
                i += 1;
            }
            i += 1;
        }
    }

    pub fn parse_file(&mut self, path: &str) {
        self.sources.push(ConfigSource::ConfigFile);
        if let Ok(content) = std::fs::read_to_string(path) {
            for (line_index, source_line) in content.lines().enumerate() {
                let line_number = line_index + 1;
                let source_content = source_line.trim().to_string();
                let line = source_content.as_str();
                if line.is_empty()
                    || line.starts_with('#')
                    || line.starts_with('[')
                    || line.starts_with(';')
                {
                    continue;
                }
                let (name, value) = match line.find('=') {
                    Some(eq_pos) => (line[..eq_pos].trim(), line[eq_pos + 1..].trim()),
                    None => (line, ""),
                };
                if name.is_empty() {
                    continue;
                }

                let definition = self.registry.get(name);
                if let Some(definition) = definition {
                    if !definition.is_supported() {
                        tracing::warn!(
                            "Ignoring unsupported option '{}' in config file '{}'",
                            name,
                            path
                        );
                        continue;
                    }
                    // aria2 config files allow boolean flags without `=true`.
                    // Keep this compatibility at the file boundary only; CLI
                    // and RPC inputs retain their stricter value contracts.
                    let value =
                        if !line.contains('=') && definition.opt_type() == OptionType::Boolean {
                            "true"
                        } else {
                            value
                        };
                    let error_start = self.errors.len();
                    self.set_raw_from(name, value, ConfigSource::ConfigFile);
                    for error_index in error_start..self.errors.len() {
                        self.error_contexts.insert(
                            error_index,
                            ConfigErrorContext {
                                line: line_number,
                                content: source_content.clone(),
                            },
                        );
                    }
                } else {
                    tracing::warn!("Unknown option '{}' in config file '{}'", name, path);
                    if line.contains('=') {
                        self.set_raw_from(name, value, ConfigSource::ConfigFile);
                    }
                }
            }
        }
    }

    pub fn parse_env_vars(&mut self) {
        self.sources.push(ConfigSource::Environment);
        for (key, value) in std::env::vars() {
            if let Some(rest) = key.strip_prefix("ARIA2_") {
                let opt_name = rest.to_lowercase().replace('_', "-");
                self.set_raw_from(opt_name, &value, ConfigSource::Environment);
            }
        }
    }

    /// Parse standard proxy environment variables and map them to aria2 options.
    ///
    /// Reads these env vars (lowercase and uppercase variants):
    /// - `http_proxy` / `HTTP_PROXY`  → `http-proxy`
    /// - `https_proxy` / `HTTPS_PROXY` → `https-proxy`
    /// - `ftp_proxy` / `FTP_PROXY`   → `ftp-proxy`
    /// - `all_proxy` / `ALL_PROXY`   → `all-proxy`
    /// - `no_proxy` / `NO_PROXY`     → `no-proxy`
    ///
    /// This should be called AFTER config file loading and BEFORE CLI arg parsing.
    pub fn parse_proxy_env_vars(&mut self) {
        self.sources.push(ConfigSource::Environment);
        let proxy_vars: &[(&str, &str)] = &[
            ("http_proxy", "http-proxy"),
            ("https_proxy", "https-proxy"),
            ("ftp_proxy", "ftp-proxy"),
            ("all_proxy", "all-proxy"),
            ("no_proxy", "no-proxy"),
        ];
        for (env_name, opt_name) in proxy_vars {
            let value = std::env::var(env_name)
                .or_else(|_| std::env::var(env_name.to_uppercase()))
                .unwrap_or_default();
            if !value.is_empty() {
                self.set_raw(*opt_name, &value);
            }
        }
    }

    pub fn apply_defaults(&mut self) {
        self.sources.push(ConfigSource::Defaults);
        for def in self.registry.all().values() {
            if !matches!(def.default_value(), OptionValue::None)
                && let Some(default_value) = def.parse_default_value()
            {
                let name = def.name().to_string();
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.options.entry(name.clone())
                {
                    entry.insert(default_value);
                    self.option_sources.insert(name, ConfigSource::Defaults);
                }
            }
        }
    }

    pub fn load_defaults_first(&mut self) {
        self.apply_defaults();
        // Config file is loaded BEFORE env vars so that env vars can override
        let conf_path = self
            .get_str("conf-path")
            .map(|s| s.to_string())
            .unwrap_or_default();
        if !conf_path.is_empty() && std::path::Path::new(&conf_path).exists() {
            self.parse_file(&conf_path);
        }
        // Env vars override config file values (higher priority)
        self.parse_env_vars();
        // Generic proxy env vars (lowest priority env source)
        self.parse_proxy_env_vars();
    }

    pub fn options(&self) -> &HashMap<String, OptionValue> {
        &self.options
    }
    pub fn errors(&self) -> &[ConfigError] {
        &self.errors
    }

    pub fn error_context(&self, index: usize) -> Option<&ConfigErrorContext> {
        self.error_contexts.get(&index)
    }
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    pub fn to_json_map(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (k, v) in &self.options {
            map.insert(
                k.clone(),
                <&OptionValue as Into<serde_json::Value>>::into(v),
            );
        }
        serde_json::Value::Object(map)
    }
}

impl Default for ConfigParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
