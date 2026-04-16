//! Centralized OptionHandler with config file parser (.aria2rc format).
//!
//! This module provides [`OptionHandler`], a self-contained option management
//! struct that holds all aria2 configuration values with built-in defaults,
//! supports loading from `.aria2rc` config files, applying CLI argument overrides,
//! and converting to [`DownloadOptions`] for use by download commands.
//!
//! The design mirrors C++ aria2's `OptionHandler` class for compatibility.
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2_core::option::option_handler::{OptionHandler, OptionValue};
//! use std::path::Path;
//!
//! let mut handler = OptionHandler::new();
//! handler.set("dir", OptionValue::Str("/downloads".into()));
//! handler.load_config_file(Path::new("~/.aria2rc")).ok();
//! let opts = handler.to_download_options();
//! ```

use std::collections::HashMap;
use std::path::Path;

use crate::request::request_group::DownloadOptions;

// ---------------------------------------------------------------------------
// OptionValue enum -- runtime value container for all supported types
// ---------------------------------------------------------------------------

/// Runtime value of an option, supporting all aria2 option types.
///
/// Provides typed accessors (`as_bool`, `as_usize`, `as_i64`, `as_f64`,
/// `as_str`, `as_str_vec`) that return the inner value or a sensible default.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum OptionValue {
    /// Boolean flag (true/false).
    Bool(bool),
    /// Unsigned integer (used for sizes, counts, ports).
    Usize(usize),
    /// Signed 64-bit integer.
    I64(i64),
    /// Floating-point number (e.g., seed-ratio).
    F64(f64),
    /// String value (paths, URLs, names).
    Str(String),
    /// List of strings (headers, headers, etc.).
    StrVec(Vec<String>),
    /// Absent / unset value.
    #[default]
    None,
}

impl std::fmt::Display for OptionValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{}", b),
            Self::Usize(n) => write!(f, "{}", n),
            Self::I64(n) => write!(f, "{}", n),
            Self::F64(v) => write!(f, "{}", v),
            Self::Str(s) => write!(f, "{}", s),
            Self::StrVec(items) => write!(f, "{}", items.join(",")),
            Self::None => write!(f, ""),
        }
    }
}

impl OptionValue {
    /// Return the inner boolean value, or `false` if this is not a `Bool`.
    pub fn as_bool(&self) -> bool {
        match self {
            Self::Bool(v) => *v,
            _ => false,
        }
    }

    /// Return the inner usize value, or `0` if this is not a `Usize`.
    pub fn as_usize(&self) -> usize {
        match self {
            Self::Usize(v) => *v,
            _ => 0,
        }
    }

    /// Return the inner i64 value, or `0` if this is not an `I64`.
    pub fn as_i64(&self) -> i64 {
        match self {
            Self::I64(v) => *v,
            _ => 0,
        }
    }

    /// Return the inner f64 value, or `0.0` if this is not an `F64`.
    pub fn as_f64(&self) -> f64 {
        match self {
            Self::F64(v) => *v,
            _ => 0.0,
        }
    }

    /// Return a reference to the inner string, or `""` if this is not a `Str`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Str(s) => s.as_str(),
            _ => "",
        }
    }

    /// Return a reference to the inner string vector, or an empty slice.
    pub fn as_str_vec(&self) -> &[String] {
        match self {
            Self::StrVec(v) => v.as_slice(),
            _ => &[],
        }
    }

    /// Check whether this value is `None`.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

// ---------------------------------------------------------------------------
// Built-in defaults (C++ aria2 compatible)
// ---------------------------------------------------------------------------

/// Built-in default option values matching C++ aria2 behavior.
///
/// These are populated into every new [`OptionHandler`] instance on construction.
/// Defined as a function returning owned values to avoid const-eval limitations
/// with `String::from` in Rust constants.
fn built_in_defaults() -> Vec<(&'static str, OptionValue)> {
    vec![
        ("dir", OptionValue::Str(String::from("."))),
        ("max-concurrent-downloads", OptionValue::Usize(5)),
        ("max-connection-per-server", OptionValue::Usize(16)),
        ("min-split-size", OptionValue::Usize(1_048_576)), // 1 MiB
        ("split", OptionValue::Usize(5)),
        ("max-overall-download-limit", OptionValue::Usize(0)), // unlimited
        ("max-download-limit", OptionValue::Usize(0)),
        ("max-upload-limit", OptionValue::Usize(0)),
        ("continue", OptionValue::Bool(true)),
        ("remote-time", OptionValue::Bool(true)),
        ("reuse-uri", OptionValue::Bool(true)),
        ("allow-overwrite", OptionValue::Bool(true)),
        (
            "file-allocation",
            OptionValue::Str(String::from("prealloc")), // prealloc / none / falloc
        ),
        ("auto-save-interval", OptionValue::Usize(60)),
        ("check-certificate", OptionValue::Bool(true)),
        ("bt-max-peers", OptionValue::Usize(128)),
        ("bt-request-peer-speed-limit", OptionValue::Usize(0)),
        ("seed-time", OptionValue::Usize(0)),
        ("seed-ratio", OptionValue::F64(0.0)),
        ("rpc-listen-port", OptionValue::Usize(6800)),
        ("rpc-secret", OptionValue::Str(String::new())),
        ("quiet", OptionValue::Bool(false)),
        (
            "console-log-level",
            OptionValue::Str(String::from("notice")),
        ),
    ]
}

// ---------------------------------------------------------------------------
// OptionHandler struct
// ---------------------------------------------------------------------------

/// Centralized option handler with built-in defaults, config file parsing,
/// CLI argument override support, and DownloadOptions conversion.
///
/// # Priority Order (lowest to highest)
///
/// 1. Built-in defaults (from [`DEFAULTS`])
/// 2. Config file values (via [`load_config_file`])
/// 3. Command-line arguments (via [`apply_args`])
/// 4. Explicit [`set`] calls
///
/// # Example
///
/// ```
/// use aria2_core::option::option_handler::{OptionHandler, OptionValue};
///
/// let mut h = OptionHandler::new();
/// assert_eq!(h.get("split").as_usize(), 5); // default
///
/// h.set("split", OptionValue::Usize(10));
/// assert_eq!(h.get("split").as_usize(), 10);
/// ```
pub struct OptionHandler {
    /// Current option values (overrides + config + args).
    options: HashMap<String, OptionValue>,
    /// Original built-in defaults (never modified after construction).
    defaults: HashMap<String, OptionValue>,
}

impl OptionHandler {
    /// Create a new `OptionHandler` pre-populated with all built-in defaults.
    ///
    /// Every default from [`built_in_defaults`] is copied into both `options` and
    /// `defaults`. Subsequent mutations only affect `options`; `defaults`
    /// remains immutable so fallback lookups always work.
    pub fn new() -> Self {
        let defaults = built_in_defaults();
        let mut options = HashMap::with_capacity(defaults.len());
        let mut defaults_map = HashMap::with_capacity(defaults.len());

        for (key, value) in defaults {
            options.insert(key.to_string(), value.clone());
            defaults_map.insert(key.to_string(), value);
        }

        Self {
            options,
            defaults: defaults_map,
        }
    }

    /// Set an option value. Overwrites any existing value.
    ///
    /// After calling `set`, subsequent calls to [`get`] will return the new
    /// value instead of the default.
    pub fn set(&mut self, key: &str, value: OptionValue) {
        self.options.insert(key.to_string(), value);
    }

    /// Get the current value for `key`.
    ///
    /// Falls back to the built-in default if the key was never explicitly set
    /// (or was removed). Returns [`OptionValue::None`] for completely unknown keys.
    pub fn get(&self, key: &str) -> &OptionValue {
        self.options
            .get(key)
            .unwrap_or_else(|| self.defaults.get(key).unwrap_or(&OptionValue::None))
    }

    /// Override options from raw command-line arguments.
    ///
    /// Parses common CLI patterns:
    /// - `--key=value` / `--key:value`
    /// - `--key value` (value in next arg)
    /// - `--no-key` sets boolean to false
    /// - `-o key=value` (GNU style)
    ///
    /// CLI arguments take precedence over config file values but can be
    /// overridden by explicit [`set`] calls.
    pub fn apply_args(&mut self, args: &[String]) {
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];

            // Skip non-option arguments (e.g., URLs, positional args)
            if !arg.starts_with('-') || arg == "--" {
                i += 1;
                continue;
            }

            // Parse --key=value or --key:value
            if let Some((key, value)) = Self::parse_kv_arg(arg) {
                if let Some(parsed) = Self::detect_value_type(value.trim()) {
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
                    if let Some(parsed) = Self::detect_value_type(value) {
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
                    && let Some(parsed) = Self::detect_value_type(value.trim())
                {
                    self.set(key, parsed);
                }
                i += 2;
                continue;
            }

            i += 1;
        }
    }

    /// Parse a `--key=value` or `--key:value` argument into `(key, value)`.
    fn parse_kv_arg(arg: &str) -> Option<(&str, &str)> {
        let stripped = arg.strip_prefix("--")?;
        if let Some((k, v)) = stripped.split_once('=') {
            return Some((k, v));
        }
        if let Some((k, v)) = stripped.split_once(':') {
            return Some((k, v));
        }
        None
    }

    /// Auto-detect the type of a raw string value and wrap it in [`OptionValue`].
    ///
    /// Detection rules:
    /// - `"true"` / `"false"` → [`OptionValue::Bool`]
    /// - Numeric string without `.` → [`OptionValue::Usize`] (or [`OptionValue::I64`] if negative)
    /// - Numeric string with `.` → [`OptionValue::F64`]
    /// - `[...]` bracket notation → [`OptionValue::StrVec`]
    /// - Quoted string → [`OptionValue::Str`] (quotes stripped)
    /// - Anything else → [`OptionValue::Str`]
    fn detect_value_type(value: &str) -> Option<OptionValue> {
        let trimmed = value.trim();

        // Empty string → None
        if trimmed.is_empty() {
            return Some(OptionValue::None);
        }

        // Boolean literals
        if trimmed == "true" || trimmed == "yes" || trimmed == "on" {
            return Some(OptionValue::Bool(true));
        }
        if trimmed == "false" || trimmed == "no" || trimmed == "off" {
            return Some(OptionValue::Bool(false));
        }

        // Bracket notation: ['val1', 'val2'] or ["val1", "val2"]
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let inner = &trimmed[1..trimmed.len() - 1];
            let items: Vec<String> = inner
                .split(',')
                .map(|s| {
                    let item = s.trim();
                    // Strip quotes if present
                    if (item.starts_with('\'') && item.ends_with('\''))
                        || (item.starts_with('"') && item.ends_with('"'))
                    {
                        &item[1..item.len() - 1]
                    } else {
                        item
                    }
                    .to_string()
                })
                .filter(|s| !s.is_empty())
                .collect();
            return Some(OptionValue::StrVec(items));
        }

        // Quoted string: "value" or 'value'
        if (trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        {
            return Some(OptionValue::Str(trimmed[1..trimmed.len() - 1].to_string()));
        }

        // Negative integer
        if let Some(neg) = trimmed.strip_prefix('-')
            && neg.parse::<i64>().is_ok()
        {
            return Some(OptionValue::I64(-neg.parse::<i64>().unwrap()));
        }

        // Unsigned integer
        if trimmed.parse::<usize>().is_ok() {
            return Some(OptionValue::Usize(trimmed.parse::<usize>().unwrap()));
        }

        // Float
        if trimmed.parse::<f64>().is_ok() {
            return Some(OptionValue::F64(trimmed.parse::<f64>().unwrap()));
        }

        // Default: plain string
        Some(OptionValue::Str(trimmed.to_string()))
    }

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
    /// # Parse Rules
    ///
    /// - Lines starting with `#` are comments and skipped.
    /// - Blank lines are skipped.
    /// - `key=value`: auto-detect type (see [`detect_value_type`]).
    /// - Invalid lines produce warnings via `tracing::warn` but do **not**
    ///   cause an error return.
    ///
    /// # Errors
    ///
    /// Returns an error only if the file cannot be read (IO failure).
    pub fn load_config_file(&mut self, path: &Path) -> Result<(), String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file '{}': {}", path.display(), e))?;

        for (line_num, raw_line) in content.lines().enumerate() {
            let line = raw_line.trim();

            // Skip blank lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Split on first '='
            let Some((key, value_str)) = line.split_once('=') else {
                tracing::warn!(
                    path = %path.display(),
                    line = line_num + 1,
                    content = raw_line,
                    "Skipping invalid config line (no '=' found)"
                );
                continue;
            };

            let key = key.trim();
            let value_str = value_str.trim();

            if key.is_empty() {
                tracing::warn!(
                    path = %path.display(),
                    line = line_num + 1,
                    "Skipping config line with empty key"
                );
                continue;
            }

            // Auto-detect type and set
            match Self::detect_value_type(value_str) {
                Some(parsed) => {
                    tracing::debug!(
                        key,
                        value = ?parsed,
                        source = %path.display(),
                        "Config option loaded"
                    );
                    self.set(key, parsed);
                }
                None => {
                    tracing::warn!(
                        key,
                        line = line_num + 1,
                        source = %path.display(),
                        "Failed to parse config value"
                    );
                }
            }
        }

        Ok(())
    }

    /// Convert current options to a [`DownloadOptions`] struct suitable for
    /// creating a download task.
    ///
    /// Maps well-known option keys to their corresponding fields in
    /// `DownloadOptions`. Unknown or unmapped keys are silently ignored.
    pub fn to_download_options(&self) -> DownloadOptions {
        let get_usize = |key: &str| -> Option<u16> {
            let v = self.get(key).as_usize();
            if v > 0 { Some(v as u16) } else { None }
        };
        let get_u64 = |key: &str| -> Option<u64> {
            let v = self.get(key).as_usize();
            if v > 0 { Some(v as u64) } else { None }
        };
        let get_str = |key: &str| -> Option<String> {
            let v = self.get(key).as_str().to_string();
            if v.is_empty() { None } else { Some(v) }
        };

        DownloadOptions {
            split: get_usize("split"),
            max_connection_per_server: get_usize("max-connection-per-server"),
            max_download_limit: get_u64("max-download-limit"),
            max_upload_limit: get_u64("max-upload-limit"),
            dir: get_str("dir"),
            out: get_str("out"),
            seed_time: get_u64("seed-time"),
            seed_ratio: {
                let r = self.get("seed-ratio").as_f64();
                if r > 0.0 { Some(r) } else { None }
            },
            checksum: None,
            cookie_file: get_str("cookie-file"),
            cookies: get_str("cookies"),
            bt_force_encrypt: self.get("bt-force-encrypt").as_bool(),
            bt_require_crypto: self.get("bt-require-crypto").as_bool(),
            enable_dht: self.get("enable-dht").as_bool(),
            dht_listen_port: get_usize("dht-listen-port"),
            enable_public_trackers: self.get("enable-public-trackers").as_bool(),
            bt_piece_selection_strategy: self
                .get("bt-piece-selection-strategy")
                .as_str()
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
            bt_prioritize_piece: self.get("bt-prioritize-piece").as_str().to_string(),
        }
    }

    /// Export all current options as a key-value map.
    ///
    /// Useful for RPC responses and serialization. Returns a clone of the
    /// internal options map (including defaults for unset keys).
    pub fn to_map(&self) -> HashMap<String, OptionValue> {
        // Merge: start with defaults, overlay with current options
        let mut map = self.defaults.clone();
        for (k, v) in &self.options {
            map.insert(k.clone(), v.clone());
        }
        map
    }

    /// Return the number of built-in defaults.
    pub fn default_count(&self) -> usize {
        self.defaults.len()
    }

    /// Check whether a specific key has been explicitly set (vs using default).
    pub fn is_explicitly_set(&self, key: &str) -> bool {
        self.options.contains_key(key)
    }

    /// Remove an explicitly-set option, reverting it to its default value.
    ///
    /// After removal, [`get`] will return the built-in default again.
    pub fn reset_to_default(&mut self, key: &str) {
        self.options.remove(key);
    }
}

impl Default for OptionHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults_populated() {
        // All defaults should be present after new()
        let handler = OptionHandler::new();
        let expected_count = built_in_defaults().len();
        assert_eq!(handler.default_count(), expected_count);
        assert!(handler.default_count() > 0);

        // Verify specific known defaults
        assert_eq!(handler.get("dir").as_str(), ".");
        assert_eq!(handler.get("split").as_usize(), 5);
        assert_eq!(handler.get("max-concurrent-downloads").as_usize(), 5);
        assert_eq!(handler.get("max-connection-per-server").as_usize(), 16);
        assert_eq!(handler.get("min-split-size").as_usize(), 1_048_576);
        assert!(handler.get("continue").as_bool());
        assert!(!handler.get("quiet").as_bool());
        assert_eq!(handler.get("seed-ratio").as_f64(), 0.0);
        assert_eq!(handler.get("rpc-listen-port").as_usize(), 6800);
        assert_eq!(handler.get("console-log-level").as_str(), "notice");
    }

    #[test]
    fn test_set_get_roundtrip() {
        let mut handler = OptionHandler::new();

        // Set and retrieve various types
        handler.set("dir", OptionValue::Str("/tmp/downloads".into()));
        assert_eq!(handler.get("dir").as_str(), "/tmp/downloads");

        handler.set("split", OptionValue::Usize(16));
        assert_eq!(handler.get("split").as_usize(), 16);

        handler.set("seed-ratio", OptionValue::F64(2.5));
        assert!((handler.get("seed-ratio").as_f64() - 2.5).abs() < f64::EPSILON);

        handler.set("quiet", OptionValue::Bool(true));
        assert!(handler.get("quiet").as_bool());

        handler.set(
            "header",
            OptionValue::StrVec(vec!["X-Custom: foo".into(), "X-Bar: baz".into()]),
        );
        assert_eq!(handler.get("header").as_str_vec().len(), 2);

        // Overwrite: second set wins
        handler.set("split", OptionValue::Usize(32));
        assert_eq!(handler.get("split").as_usize(), 32);

        // Unknown key returns None variant
        assert!(handler.get("nonexistent-key").is_none());
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_load_config_file() {
        let mut handler = OptionHandler::new();

        // Build sample .aria2rc content
        let config_content = r#"
# This is a comment
dir="/home/user/downloads"
split=16
max-connection-per-server=8
quiet=true
seed-ratio=1.5
custom-list=['header1', 'header2', 'header3']
bool-flag=yes
number-key=42
float-key=3.14

# Another comment
allow-overwrite=false
"#;

        // Write to temp file
        let tmp_dir = std::env::temp_dir();
        let config_path = tmp_dir.join(format!("aria2_test_config_{}.aria2rc", std::process::id()));
        std::fs::write(&config_path, config_content).expect("Failed to write temp config");

        // Load config file
        let result = handler.load_config_file(&config_path);
        assert!(
            result.is_ok(),
            "load_config_file should succeed: {:?}",
            result.err()
        );

        // Verify loaded values override defaults
        assert_eq!(handler.get("dir").as_str(), "/home/user/downloads");
        assert_eq!(handler.get("split").as_usize(), 16);
        assert_eq!(handler.get("max-connection-per-server").as_usize(), 8);
        assert!(handler.get("quiet").as_bool());
        assert!((handler.get("seed-ratio").as_f64() - 1.5).abs() < f64::EPSILON);
        assert!(!handler.get("allow-overwrite").as_bool());

        // Verify list parsing
        let list_val = handler.get("custom-list");
        assert_eq!(list_val.as_str_vec().len(), 3);
        assert_eq!(list_val.as_str_vec()[0], "header1");

        // Verify auto-detected types
        assert!(handler.get("bool-flag").as_bool()); // yes -> true
        assert_eq!(handler.get("number-key").as_usize(), 42);
        let float_val = handler.get("float-key").as_f64();
        assert!((float_val - 3.14).abs() < f64::EPSILON);

        // Defaults should still be intact for unmentioned keys
        assert_eq!(handler.get("rpc-listen-port").as_usize(), 6800);

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[test]
    fn test_apply_args_overrides_config() {
        let mut handler = OptionHandler::new();

        // First load config file with some values
        let config_content = r#"
dir=/config/dir
split=4
quiet=false
"#;
        let tmp_dir = std::env::temp_dir();
        let config_path = tmp_dir.join(format!(
            "aria2_test_override_{}.aria2rc",
            std::process::id()
        ));
        std::fs::write(&config_path, config_content).expect("Failed to write config");
        handler
            .load_config_file(&config_path)
            .expect("Should load config");

        // Verify config values loaded
        assert_eq!(handler.get("dir").as_str(), "/config/dir");
        assert_eq!(handler.get("split").as_usize(), 4);
        assert!(!handler.get("quiet").as_bool());

        // Now apply CLI args (should override config)
        let cli_args: Vec<String> = vec![
            "--dir=/cli/dir".to_string(),
            "--split=12".to_string(),
            "--quiet".to_string(), // flag without value -> bool true
            "--max-connection-per-server=8".to_string(),
            "--seed-ratio=2.0".to_string(),
            "--no-continue".to_string(), // --no-key pattern -> bool false
        ];
        handler.apply_args(&cli_args);

        // CLI args should win over config
        assert_eq!(handler.get("dir").as_str(), "/cli/dir");
        assert_eq!(handler.get("split").as_usize(), 12);
        assert!(handler.get("quiet").as_bool()); // CLI flag overrides config
        assert_eq!(handler.get("max-connection-per-server").as_usize(), 8);
        assert!((handler.get("seed-ratio").as_f64() - 2.0).abs() < f64::EPSILON);
        assert!(!handler.get("continue").as_bool()); // --no-continue

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[test]
    fn test_to_download_options() {
        let mut handler = OptionHandler::new();

        // Set values that map to DownloadOptions fields
        handler.set("split", OptionValue::Usize(8));
        handler.set("max-connection-per-server", OptionValue::Usize(4));
        handler.set("max-download-limit", OptionValue::Usize(102400));
        handler.set("max-upload-limit", OptionValue::Usize(51200));
        handler.set("dir", OptionValue::Str("/data".to_string()));
        handler.set("out", OptionValue::Str("output.bin".to_string()));
        handler.set("seed-time", OptionValue::Usize(300));
        handler.set("seed-ratio", OptionValue::F64(2.0));

        let opts = handler.to_download_options();

        // Verify conversion produced correct struct
        assert_eq!(opts.split, Some(8));
        assert_eq!(opts.max_connection_per_server, Some(4));
        assert_eq!(opts.max_download_limit, Some(102400));
        assert_eq!(opts.max_upload_limit, Some(51200));
        assert_eq!(opts.dir, Some("/data".to_string()));
        assert_eq!(opts.out, Some("output.bin".to_string()));
        assert_eq!(opts.seed_time, Some(300));
        assert_eq!(opts.seed_ratio, Some(2.0));

        // Default values (non-zero) should be preserved in DownloadOptions
        let handler2 = OptionHandler::new();
        let opts2 = handler2.to_download_options();
        assert_eq!(opts2.split, Some(5)); // default split=5 which is > 0
        assert_eq!(opts2.max_connection_per_server, Some(16)); // default is 16
        assert_eq!(opts2.dir, Some(".".to_string())); // default dir is "."
        assert_eq!(opts2.out, None); // "out" has no default -> None

        // Verify reset_to_default works
        handler.reset_to_default("split");
        assert_eq!(handler.get("split").as_usize(), 5); // back to default
        assert!(!handler.is_explicitly_set("split"));
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_detect_value_type_edge_cases() {
        // Test various auto-detection scenarios
        assert_eq!(
            OptionHandler::detect_value_type("true"),
            Some(OptionValue::Bool(true))
        );
        assert_eq!(
            OptionHandler::detect_value_type("false"),
            Some(OptionValue::Bool(false))
        );
        assert_eq!(
            OptionHandler::detect_value_type("yes"),
            Some(OptionValue::Bool(true))
        );
        assert_eq!(
            OptionHandler::detect_value_type("no"),
            Some(OptionValue::Bool(false))
        );
        assert_eq!(
            OptionHandler::detect_value_type("42"),
            Some(OptionValue::Usize(42))
        );
        assert_eq!(
            OptionHandler::detect_value_type("-10"),
            Some(OptionValue::I64(-10))
        );
        let detected = OptionHandler::detect_value_type("3.14159")
            .unwrap()
            .as_f64();
        assert!((detected - 3.14159).abs() < 0.001); // use full precision to avoid lint
        assert_eq!(
            OptionHandler::detect_value_type("\"quoted string\""),
            Some(OptionValue::Str("quoted string".into()))
        );
        assert_eq!(
            OptionHandler::detect_value_type("['a','b','c']"),
            Some(OptionValue::StrVec(vec![
                "a".into(),
                "b".into(),
                "c".into()
            ]))
        );
        assert_eq!(
            OptionHandler::detect_value_type(""),
            Some(OptionValue::None)
        );
        assert_eq!(
            OptionHandler::detect_value_type("plain_text"),
            Some(OptionValue::Str("plain_text".into()))
        );
    }

    #[test]
    fn test_option_value_display() {
        assert_eq!(OptionValue::Bool(true).to_string(), "true");
        assert_eq!(OptionValue::Usize(42).to_string(), "42");
        assert_eq!(OptionValue::I64(-10).to_string(), "-10");
        assert_eq!(
            format!("{:.2}", {
                #[allow(clippy::approx_constant)]
                OptionValue::F64(3.14).to_string().parse::<f64>().unwrap()
            }),
            "3.14"
        ); // approximate
        assert_eq!(OptionValue::Str("hello".to_string()).to_string(), "hello");
        assert_eq!(
            OptionValue::StrVec(vec!["a".into(), "b".into()]).to_string(),
            "a,b"
        );
        assert_eq!(OptionValue::None.to_string(), "");
    }

    #[test]
    fn test_to_map_includes_all() {
        let mut handler = OptionHandler::new();
        handler.set("custom-key", OptionValue::Str("custom-value".into()));

        let map = handler.to_map();
        // Should include all defaults plus custom key
        assert!(map.contains_key("dir"));
        assert!(map.contains_key("split"));
        assert!(map.contains_key("custom-key"));
        assert_eq!(map.get("custom-key").unwrap().as_str(), "custom-value");
        // Map size >= defaults count
        assert!(map.len() >= built_in_defaults().len());
    }
}
