//! Centralized OptionHandler with config file parser (.aria2rc format).
//!
//! This module provides [`OptionHandler`], a self-contained option management
//! struct that holds all aria2 configuration values with built-in defaults,
//! supports loading from `.aria2rc` config files, applying CLI argument overrides,
//! and converting to [`DownloadOptions`] for use by download commands.
//!
//! The design mirrors C++ aria2's `OptionHandler` class for reference.
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2_core::option::{OptionHandler, OptionValue};
//! use aria2_core::option::option_handler::OptionHandlerApply;
//! use std::path::Path;
//!
//! let mut handler = OptionHandler::new();
//! handler.set("dir", OptionValue::Str("/downloads".into()));
//! handler.load_config_file(Path::new("~/.aria2rc")).ok();
//! let opts = handler.to_download_options();
//! ```

mod apply;
mod parsing;
mod tests;
mod validation;

pub use apply::OptionHandlerApply;
pub use parsing::{detect_value_type, parse_kv_arg};
pub use validation::parse_config_line;

use std::collections::HashMap;

use crate::config::option::OptionValue;

// OptionValue is now defined in crate::config::option and re-exported
// from crate::option for backward compatibility.

// ---------------------------------------------------------------------------
// Built-in defaults (C++ aria2 compatible)
// ---------------------------------------------------------------------------

/// Built-in default option values matching C++ aria2 behavior.
///
/// These are populated into every new [`OptionHandler`] instance on construction.
/// Defined as a function returning owned values to avoid const-eval limitations
/// with `String::from` in Rust constants.
pub(super) fn built_in_defaults() -> Vec<(&'static str, OptionValue)> {
    vec![
        ("dir", OptionValue::Str(String::from("."))),
        ("max-concurrent-downloads", OptionValue::Usize(5)),
        ("max-connection-per-server", OptionValue::Usize(16)),
        ("min-split-size", OptionValue::Usize(1_048_576)), // 1 MiB
        ("split", OptionValue::Usize(16)),
        ("max-overall-download-limit", OptionValue::Usize(0)), // unlimited
        ("max-download-limit", OptionValue::Usize(0)),
        ("max-upload-limit", OptionValue::Usize(0)),
        ("continue", OptionValue::Bool(true)),
        ("remote-time", OptionValue::Bool(true)),
        ("reuse-uri", OptionValue::Bool(true)),
        ("allow-overwrite", OptionValue::Bool(true)),
        ("file-allocation", OptionValue::Str(String::from("trunc"))),
        (
            "mmap-threshold",
            OptionValue::Usize(256 * 1024 * 1024), // 256 MiB
        ),
        ("auto-save-interval", OptionValue::Usize(60)),
        ("check-certificate", OptionValue::Bool(true)),
        ("bt-max-peers", OptionValue::Usize(128)),
        ("bt-request-peer-speed-limit", OptionValue::Usize(0)),
        ("seed-time", OptionValue::Usize(0)),
        ("seed-ratio", OptionValue::Float(0.0)),
        ("rpc-listen-port", OptionValue::Usize(6800)),
        ("rpc-secret", OptionValue::Str(String::new())),
        ("quiet", OptionValue::Bool(false)),
        ("console-log-level", OptionValue::Str(String::from("info"))),
        // HTTP authentication defaults (C++ PREF_* compatible)
        ("http-auth-challenge", OptionValue::Bool(false)),
        ("http-user", OptionValue::Str(String::new())),
        ("http-passwd", OptionValue::Str(String::new())),
        ("ftp-user", OptionValue::Str(String::from("anonymous"))),
        ("ftp-passwd", OptionValue::Str(String::from("anonymous@"))),
        ("no-netrc", OptionValue::Bool(false)),
        ("netrc-path", OptionValue::Str(String::new())),
        ("conditional-get", OptionValue::Bool(false)),
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
/// 1. Built-in defaults (from [`built_in_defaults`])
/// 2. Config file values (via [`load_config_file`](OptionHandler::load_config_file))
/// 3. Command-line arguments (via [`apply_args`](OptionHandler::apply_args))
/// 4. Explicit [`set`](OptionHandler::set) calls
///
/// # Example
///
/// ```no_run
/// use aria2_core::option::{OptionHandler, OptionValue};
///
/// let mut h = OptionHandler::new();
/// assert_eq!(h.get("split").as_usize(), 16); // default
///
/// h.set("split", OptionValue::Usize(10));
/// assert_eq!(h.get("split").as_usize(), 10);
/// ```
pub struct OptionHandler {
    /// Current option values (overrides + config + args).
    pub(crate) options: HashMap<String, OptionValue>,
    /// Original built-in defaults (never modified after construction).
    pub(crate) defaults: HashMap<String, OptionValue>,
}

impl OptionHandler {
    /// Create a new `OptionHandler` pre-populated with all built-in defaults.
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

    /// Export all current options as a key-value map.
    pub fn to_map(&self) -> HashMap<String, OptionValue> {
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
    pub fn reset_to_default(&mut self, key: &str) {
        self.options.remove(key);
    }

    /// Auto-detect the type of a raw string value and wrap it in [`OptionValue`].
    ///
    /// Delegates to [`detect_value_type`]. Kept as an associated function
    /// for backward compatibility with existing call sites and tests.
    pub fn detect_value_type(value: &str) -> Option<OptionValue> {
        detect_value_type(value)
    }

    /// Parse a `--key=value` or `--key:value` argument into `(key, value)`.
    ///
    /// Delegates to [`parse_kv_arg`]. Kept as an associated function
    /// for backward compatibility.
    pub fn parse_kv_arg(arg: &str) -> Option<(&str, &str)> {
        parse_kv_arg(arg)
    }
}

impl Default for OptionHandler {
    fn default() -> Self {
        Self::new()
    }
}
