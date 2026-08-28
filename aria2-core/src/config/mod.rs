pub mod netrc;
pub mod option;
pub mod option_definitions;
pub mod parser;
pub mod runtime;
pub mod uri_list;

use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "bittorrent")]
pub use aria2_protocol::bittorrent::tracker::public_list::TrackerCatalogConfig;
pub use netrc::{NetRcEntry, NetRcError, NetRcFile};
pub use option::{
    ChoiceValidator, DependencyChecker, OptionCategory, OptionDef, OptionDefinition, OptionError,
    OptionOwner, OptionRegistry, OptionType, OptionValidator, OptionValue, PathValidator,
    PiecePriorityRule, RangeValidator, RegexValidator, UrlValidator, parse_index_out,
    parse_integer_segments, parse_piece_priority,
};
pub use parser::{ConfigError, ConfigErrorContext, ConfigParser, ConfigSource};
pub use runtime::{
    ChangeableKind, INITIAL_IDENTITY_OPTIONS, INITIAL_REQUEST_OPTIONS,
    INITIAL_SNAPSHOT_WIRE_OPTIONS, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS,
    RUNTIME_CHANGEABLE_OPTIONS, RUNTIME_GLOBAL_CHANGEABLE_OPTIONS, is_global_option_changeable,
    is_initial_option, is_option_changeable, project_initial_options,
};
pub use uri_list::{UriListEntry, UriListError, UriListFile};

#[cfg(test)]
mod contract_tests;

/// Emitted when a global option value changes via `set_global_option`.
///
/// Subscribers can listen for these events via `ConfigManager::subscribe_changes()`.
#[derive(Debug, Clone)]
pub struct ConfigChangeEvent {
    /// The option name that was changed (e.g., "split", "dir").
    pub key: String,
    /// The previous value before the change.
    pub old_value: OptionValue,
    /// The new value after the change.
    pub new_value: OptionValue,
}

/// Unified runtime configuration manager for aria2-rust.
///
/// `ConfigManager` provides a two-tier option storage system:
/// - **Global options**: Shared across all download tasks
/// - **Task-level options**: Per-task overrides that inherit from globals
///
/// Options are loaded from four sources in priority order:
/// 1. Built-in defaults (from `OptionRegistry`)
/// 2. Environment variables (`ARIA2_*` prefix)
/// 3. Configuration file (`~/.aria2/aria2.conf`)
/// 4. Command-line arguments (highest priority)
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::config::ConfigManager;
/// use aria2_core::config::OptionValue;
///
/// #[tokio::main]
/// async fn main() {
///     let mut mgr = ConfigManager::new();
///     mgr.set_global_option("split", OptionValue::Int(8)).await.unwrap();
///     assert_eq!(mgr.get_global_i64("split").await, Some(8));
/// }
/// ```
pub struct ConfigManager {
    global_opts: Arc<tokio::sync::RwLock<HashMap<String, OptionValue>>>,
    task_defaults: Arc<tokio::sync::RwLock<HashMap<String, HashMap<String, OptionValue>>>>,
    parser: ConfigParser,
    registry: OptionRegistry,
    change_tx: tokio::sync::broadcast::Sender<ConfigChangeEvent>,
}

impl ConfigManager {
    /// Create a new `ConfigManager` with the built-in `OptionRegistry`
    /// containing ~95 core aria2 options.
    pub fn new() -> Self {
        let (change_tx, _) = tokio::sync::broadcast::channel(64);
        let registry = OptionRegistry::new();
        let mut parser = ConfigParser::with_registry(registry.clone());
        parser.load_defaults_first();
        Self {
            global_opts: Arc::new(tokio::sync::RwLock::new(parser.options().clone())),
            task_defaults: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            parser,
            registry,
            change_tx,
        }
    }

    /// Create a configuration manager with identity defaults supplied by an
    /// embedding product. Standalone core users should use [`Self::new`].
    pub fn new_with_identity(user_agent: impl Into<String>, peer_agent: impl Into<String>) -> Self {
        let mut registry = OptionRegistry::new();
        registry
            .set_default_value("user-agent", OptionValue::Str(user_agent.into()))
            .expect("the built-in user-agent option must be registered");

        // `peer-agent` is feature-gated in the core registry, so a non-
        // BitTorrent build simply keeps the standalone registry shape.
        let _ = registry.set_default_value("peer-agent", OptionValue::Str(peer_agent.into()));
        Self::new_with_registry(registry)
    }

    /// Create a `ConfigManager` with a custom `OptionRegistry`.
    ///
    /// Use this when you need to register custom options beyond the
    /// built-in set (e.g., application-specific configuration).
    pub fn new_with_registry(registry: OptionRegistry) -> Self {
        Self::new_with_registry_and_config(registry, true)
    }

    /// Create a manager with defaults but without reading a configuration
    /// file. Standalone applications use this when they must honor
    /// command-line configuration controls such as no-conf.
    pub fn new_with_registry_without_config(registry: OptionRegistry) -> Self {
        Self::new_with_registry_and_config(registry, false)
    }

    fn new_with_registry_and_config(registry: OptionRegistry, load_config_file: bool) -> Self {
        let (change_tx, _) = tokio::sync::broadcast::channel(64);
        let mut parser = ConfigParser::with_registry(registry.clone());
        if load_config_file {
            parser.load_defaults_first();
        } else {
            parser.apply_defaults();
        }
        Self {
            global_opts: Arc::new(tokio::sync::RwLock::new(parser.options().clone())),
            task_defaults: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            parser,
            registry,
            change_tx,
        }
    }

    /// Identity-aware constructor that does not implicitly read a config file.
    pub fn new_with_identity_without_config(
        user_agent: impl Into<String>,
        peer_agent: impl Into<String>,
    ) -> Self {
        let mut registry = OptionRegistry::new();
        registry
            .set_default_value("user-agent", OptionValue::Str(user_agent.into()))
            .expect("the built-in user-agent option must be registered");
        let _ = registry.set_default_value("peer-agent", OptionValue::Str(peer_agent.into()));
        Self::new_with_registry_without_config(registry)
    }

    /// Parse and load command-line arguments into global options.
    ///
    /// Supports formats: `--opt=val`, `--opt val`, `-o val`, `--no-opt`,
    /// and `@file` for URI list file references.
    pub async fn load_cli(&mut self, args: &[String]) {
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.parser.parse_cli_args(&args_ref);
        self.sync_global().await;
    }

    /// Load options from an aria2.conf-format configuration file.
    pub async fn load_file(&mut self, path: &str) {
        self.parser.parse_file(path);
        self.sync_global().await;
    }

    /// Load options from environment variables with `ARIA2_` prefix.
    ///
    /// Maps `ARIA2_SPLIT` → `split`, `ARIA2_DIR` → `dir`, etc.
    pub async fn load_env(&mut self) {
        self.parser.parse_env_vars();
        self.sync_global().await;
    }

    async fn sync_global(&self) {
        let mut opts = self.global_opts.write().await;
        for (k, v) in self.parser.options() {
            opts.insert(k.clone(), v.clone());
        }
    }

    /// Get a global option value by name.
    pub async fn get_global_option(&self, name: &str) -> Option<OptionValue> {
        self.global_opts
            .read()
            .await
            .get(OptionRegistry::canonical_name(name))
            .cloned()
    }

    /// Convenience: get a global option as a `String`.
    ///
    /// Returns `None` if the option doesn't exist or is not a string type.
    pub async fn get_global_str(&self, name: &str) -> Option<String> {
        self.global_opts
            .read()
            .await
            .get(OptionRegistry::canonical_name(name))
            .and_then(|v| match v {
                OptionValue::Str(s) => Some(s.clone()),
                _ => None,
            })
    }

    /// Convenience: get a global option as an `i64` integer.
    ///
    /// Returns `None` if the option doesn't exist or is not an integer type.
    pub async fn get_global_i64(&self, name: &str) -> Option<i64> {
        self.global_opts
            .read()
            .await
            .get(OptionRegistry::canonical_name(name))
            .and_then(|v| match v {
                OptionValue::Int(n) => Some(*n),
                _ => None,
            })
    }

    /// Convenience: get a global option as a `bool`.
    ///
    /// Returns `None` if the option doesn't exist or is not a boolean type.
    pub async fn get_global_bool(&self, name: &str) -> Option<bool> {
        self.global_opts
            .read()
            .await
            .get(OptionRegistry::canonical_name(name))
            .and_then(|v| match v {
                OptionValue::Bool(b) => Some(*b),
                _ => None,
            })
    }

    /// Set a global option value with validation.
    ///
    /// Validates against the `OptionRegistry` (type checking, range validation).
    /// Emits a `ConfigChangeEvent` on success. Returns an error for unknown
    /// options or validation failures.
    pub async fn set_global_option(
        &mut self,
        name: &str,
        value: OptionValue,
    ) -> Result<(), String> {
        if !self.registry.contains(name) {
            return Err(format!("unknown option '{}'", name));
        }
        let canonical_name = OptionRegistry::canonical_name(name).to_string();
        let def = self.registry.get(&canonical_name).unwrap();
        let parsed = def.parse_value(&value.to_string())?;
        let old = self.global_opts.read().await.get(&canonical_name).cloned();
        {
            let mut opts = self.global_opts.write().await;
            opts.insert(canonical_name.clone(), parsed.clone());
        }
        self.parser.set(canonical_name.clone(), value);
        let _ = self.change_tx.send(ConfigChangeEvent {
            key: canonical_name,
            old_value: old.unwrap_or(OptionValue::None),
            new_value: parsed,
        });
        Ok(())
    }

    /// Batch-set multiple global options (RPC `changeGlobalOption` compatible).
    ///
    /// Returns a list of error messages for each failed option.
    /// Options that succeed are applied immediately.
    pub async fn change_global_options(&mut self, options: HashMap<String, String>) -> Vec<String> {
        let mut errors = Vec::new();
        for (key, value) in options {
            if let Err(e) = self.set_global_option(&key, OptionValue::Str(value)).await {
                errors.push(e);
            }
        }
        errors
    }

    pub async fn get_all_global_options(&self) -> HashMap<String, OptionValue> {
        self.global_opts.read().await.clone()
    }

    pub async fn get_all_global_options_json(&self) -> serde_json::Value {
        let opts = self.global_opts.read().await;
        let mut map = serde_json::Map::new();
        for (k, v) in opts.iter() {
            map.insert(
                k.clone(),
                <&OptionValue as Into<serde_json::Value>>::into(v),
            );
        }
        serde_json::Value::Object(map)
    }

    pub async fn get_task_default(&self, gid: &str, name: &str) -> Option<OptionValue> {
        let canonical_name = OptionRegistry::canonical_name(name);
        let tasks = self.task_defaults.read().await;
        let task_val = tasks.get(gid).and_then(|m| m.get(canonical_name)).cloned();
        if task_val.is_some() {
            return task_val;
        }
        drop(tasks);
        self.global_opts.read().await.get(canonical_name).cloned()
    }

    pub async fn set_task_option(
        &mut self,
        gid: &str,
        name: &str,
        value: OptionValue,
    ) -> Result<(), String> {
        if !self.registry.contains(name) {
            return Err(format!("unknown option '{}'", name));
        }
        let canonical_name = OptionRegistry::canonical_name(name).to_string();
        let def = self.registry.get(&canonical_name).unwrap();
        let parsed = def.parse_value(&value.to_string())?;
        let mut tasks = self.task_defaults.write().await;
        let entry = tasks.entry(gid.to_string()).or_insert_with(HashMap::new);
        entry.insert(canonical_name, parsed);
        Ok(())
    }

    pub async fn change_task_options(
        &mut self,
        gid: &str,
        options: HashMap<String, String>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for (key, value) in options {
            if let Err(e) = self
                .set_task_option(gid, &key, OptionValue::Str(value))
                .await
            {
                errors.push(e);
            }
        }
        errors
    }

    pub async fn get_task_options(&self, gid: &str) -> HashMap<String, OptionValue> {
        let tasks = self.task_defaults.read().await;
        let global = self.global_opts.read().await;
        if let Some(task_opts) = tasks.get(gid) {
            let mut merged = global.clone();
            for (k, v) in task_opts {
                merged.insert(k.clone(), v.clone());
            }
            merged
        } else {
            global.clone()
        }
    }

    pub async fn remove_task(&mut self, gid: &str) {
        let mut tasks = self.task_defaults.write().await;
        tasks.remove(gid);
    }

    /// Subscribe to configuration change events.
    ///
    /// Returns a `broadcast::Receiver` that receives `ConfigChangeEvent`
    /// whenever `set_global_option` is called.
    pub fn subscribe_changes(&self) -> tokio::sync::broadcast::Receiver<ConfigChangeEvent> {
        self.change_tx.subscribe()
    }

    pub fn registry(&self) -> &OptionRegistry {
        &self.registry
    }
    pub fn parser(&self) -> &ConfigParser {
        &self.parser
    }

    /// Return the source that supplied the currently effective option value.
    ///
    /// This is intentionally read-only metadata for application startup
    /// decisions; runtime option mutation remains governed by the normal
    /// global-option APIs.
    pub async fn global_option_source(&self, name: &str) -> Option<ConfigSource> {
        self.parser.option_source(name).cloned()
    }
    pub fn has_errors(&self) -> bool {
        self.parser.has_errors()
    }
    pub fn errors(&self) -> &[ConfigError] {
        self.parser.errors()
    }

    pub async fn save_session(&self, path: &str) -> Result<(), String> {
        let opts = self.global_opts.read().await;
        let content = opts
            .iter()
            .filter_map(|(k, v)| {
                if matches!(v, OptionValue::None) {
                    None
                } else {
                    Some(format!("{}={}", k, v))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, content).map_err(|e| format!("failed to save session: {}", e))
    }

    pub async fn load_session(&mut self, path: &str) -> Result<(), String> {
        self.parser.parse_file(path);
        self.sync_global().await;
        Ok(())
    }

    pub async fn create_task_config(
        &self,
        overrides: HashMap<String, OptionValue>,
    ) -> HashMap<String, OptionValue> {
        let global = self.global_opts.read().await;
        let mut config = global.clone();
        for (k, v) in overrides {
            config.insert(k, v);
        }
        config
    }
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_manager_creation() {
        let mgr = ConfigManager::new();
        assert!(!mgr.has_errors());
        let dir = mgr.get_global_str("dir").await;
        assert!(dir.is_some());
    }

    #[tokio::test]
    async fn test_embedding_identity_overrides_only_identity_defaults() {
        let mgr = ConfigManager::new_with_identity("aria2-rust/9.9.9", "aria2-rust/9.9.9");

        assert_eq!(
            mgr.get_global_str("user-agent").await.as_deref(),
            Some("aria2-rust/9.9.9")
        );
        #[cfg(feature = "bittorrent")]
        assert_eq!(
            mgr.get_global_str("peer-agent").await.as_deref(),
            Some("aria2-rust/9.9.9")
        );
        assert_eq!(mgr.get_global_str("dir").await, Some(".".to_string()));
    }

    #[tokio::test]
    async fn test_get_and_set_global() {
        let mut mgr = ConfigManager::new();
        let result = mgr.set_global_option("split", OptionValue::Int(8)).await;
        assert!(result.is_ok());
        let val = mgr.get_global_i64("split").await;
        assert_eq!(val, Some(8));
    }

    #[tokio::test]
    async fn test_set_unknown_option_fails() {
        let mut mgr = ConfigManager::new();
        let result = mgr
            .set_global_option("nonexistent-option", OptionValue::Str("value".into()))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_set_unsupported_http_pipeline_limit_fails() {
        let mut mgr = ConfigManager::new();
        let result = mgr
            .set_global_option("max-http-pipelining", OptionValue::Int(2))
            .await;

        let error = result.expect_err("unsupported pipeline limits must be rejected");
        assert!(
            error.contains("max-http-pipelining"),
            "unexpected error: {error}"
        );
        assert_eq!(mgr.get_global_i64("max-http-pipelining").await, None);
    }

    #[tokio::test]
    async fn test_change_global_options_batch() {
        let mut mgr = ConfigManager::new();
        let mut opts = HashMap::new();
        opts.insert("split".to_string(), "10".to_string());
        opts.insert("quiet".to_string(), "true".to_string());
        let errors = mgr.change_global_options(opts).await;
        assert!(errors.is_empty());
        assert_eq!(mgr.get_global_i64("split").await, Some(10));
        assert_eq!(mgr.get_global_bool("quiet").await, Some(true));
    }

    #[tokio::test]
    async fn test_get_all_global_options() {
        let mgr = ConfigManager::new();
        let all = mgr.get_all_global_options().await;
        assert!(!all.is_empty());
        assert!(all.contains_key("dir"));
    }

    #[tokio::test]
    async fn test_get_all_global_options_json() {
        let mgr = ConfigManager::new();
        let json = mgr.get_all_global_options_json().await;
        assert!(json.is_object());
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("dir"));
    }

    #[tokio::test]
    async fn test_task_options_inherit_global() {
        let mut mgr = ConfigManager::new();
        mgr.set_global_option("split", OptionValue::Int(8))
            .await
            .unwrap();
        let task_val = mgr.get_task_default("gid-001", "split").await;
        assert_eq!(task_val.as_ref().and_then(|v| v.as_i64()), Some(8));
    }

    #[tokio::test]
    async fn test_task_options_override_global() {
        let mut mgr = ConfigManager::new();
        mgr.set_global_option("split", OptionValue::Int(5))
            .await
            .unwrap();
        mgr.set_task_option("gid-001", "split", OptionValue::Int(12))
            .await
            .unwrap();
        let task_val = mgr.get_task_default("gid-001", "split").await;
        assert_eq!(task_val.and_then(|v| v.as_i64()), Some(12));
    }

    #[tokio::test]
    async fn test_change_task_options_batch() {
        let mut mgr = ConfigManager::new();
        let mut opts = HashMap::new();
        opts.insert("out".to_string(), "special.txt".to_string());
        let errors = mgr.change_task_options("gid-002", opts).await;
        assert!(errors.is_empty());
    }

    #[tokio::test]
    async fn test_remove_task() {
        let mut mgr = ConfigManager::new();
        mgr.set_task_option("gid-003", "out", OptionValue::Str("file.txt".into()))
            .await
            .unwrap();
        mgr.remove_task("gid-003").await;
        let val = mgr.get_task_default("gid-003", "out").await;
        assert_eq!(val.map(|v| v.as_str().map(|s| s.to_string())), None);
    }

    #[tokio::test]
    async fn test_change_event_broadcast() {
        let mut mgr = ConfigManager::new();
        let mut rx = mgr.subscribe_changes();
        mgr.set_global_option("quiet", OptionValue::Bool(true))
            .await
            .unwrap();
        let event = rx.recv().await;
        assert!(event.is_ok());
        let evt = event.unwrap();
        assert_eq!(evt.key, "quiet");
    }

    #[tokio::test]
    async fn test_create_task_config_merges_overrides() {
        let mut mgr = ConfigManager::new();
        mgr.set_global_option("dir", OptionValue::Str("/global".into()))
            .await
            .unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("dir".into(), OptionValue::Str("/local".into()));
        overrides.insert("out".into(), OptionValue::Str("file.txt".into()));
        let config = mgr.create_task_config(overrides).await;
        assert_eq!(config.get("dir").and_then(|v| v.as_str()), Some("/local"));
        assert_eq!(config.get("out").and_then(|v| v.as_str()), Some("file.txt"));
    }

    #[tokio::test]
    async fn test_save_and_load_session() {
        let mut mgr = ConfigManager::new();
        mgr.set_global_option("split", OptionValue::Int(7))
            .await
            .unwrap();
        let tmp_dir = std::env::temp_dir();
        let session_path = format!(
            "{}/aria2_test_session_{}.txt",
            tmp_dir.display(),
            std::process::id()
        );
        mgr.save_session(&session_path).await.unwrap();

        let mut mgr2 = ConfigManager::new();
        mgr2.load_session(&session_path).await.unwrap();
        let val = mgr2.get_global_i64("split").await;
        assert_eq!(val, Some(7));

        let _ = std::fs::remove_file(&session_path);
    }

    #[tokio::test]
    async fn test_load_cli_args() {
        let mut mgr = ConfigManager::new();
        mgr.load_cli(&["--dir=/custom/path".to_string(), "--split=12".to_string()])
            .await;
        assert_eq!(mgr.get_global_str("dir").await, Some("/custom/path".into()));
        assert_eq!(mgr.get_global_i64("split").await, Some(12));
    }

    #[tokio::test]
    async fn test_registry_access() {
        let mgr = ConfigManager::new();
        assert!(mgr.registry().contains("split"));
        assert!(mgr.registry().get("dir").is_some());
        assert!(mgr.registry().count() >= 60);
    }
}
