//! Configuration option registry and core logic.
//!
//! This module provides [`OptionRegistry`], the central registry for all aria2
//! configuration options. Type definitions are in [`option_types`](super::option_types)
//! and built-in option registrations are in [`option_definitions`](super::option_definitions).

use std::collections::HashMap;

// Re-export all public types so that external consumers can use `config::OptionType`, etc.
pub use super::option_types::{OptionCategory, OptionDef, OptionType, OptionValue};

/// Registry of all known configuration options.
///
/// `OptionRegistry` stores metadata for every supported aria2 option (~95 built-in
/// options organized into 5 categories). It is used by [`ConfigManager`](super::ConfigManager)
/// to validate and parse option values.
///
/// # Example
///
/// ```rust
/// use aria2_core::config::OptionRegistry;
///
/// let reg = OptionRegistry::new();
/// assert!(reg.contains("split"));
/// assert!(reg.get("split").unwrap().opt_type().to_string() == "integer");
/// ```
#[derive(Clone)]
pub struct OptionRegistry {
    options: HashMap<String, OptionDef>,
}

impl OptionRegistry {
    /// Create a new `OptionRegistry` pre-populated with all built-in aria2 options.
    ///
    /// This registers all ~95 options across 5 categories:
    /// - General (directory, logging, UI)
    /// - HTTP/FTP (proxies, headers, timeouts)
    /// - BitTorrent (seeding, DHT, PEX)
    /// - RPC (server settings, authentication)
    /// - Advanced (bandwidth limits, disk cache)
    pub fn new() -> Self {
        let mut reg = Self {
            options: HashMap::new(),
        };
        // Register all categorized option groups
        reg.register_general_options();
        reg.register_http_ftp_options();
        reg.register_bt_options();
        reg.register_rpc_options();
        reg.register_advanced_options();
        reg
    }

    /// Register a single option definition into the registry.
    pub fn register(&mut self, def: OptionDef) {
        self.options.insert(def.name().to_string(), def);
    }

    /// Look up an option definition by name.
    pub fn get(&self, name: &str) -> Option<&OptionDef> {
        self.options.get(name)
    }

    /// Check whether an option with the given name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    /// Get read access to the complete option map.
    pub fn all(&self) -> &HashMap<String, OptionDef> {
        &self.options
    }

    /// Return the total number of registered options.
    pub fn count(&self) -> usize {
        self.options.len()
    }

    /// Filter options by category, returning all definitions in the given group.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_option_type_display() {
        assert_eq!(OptionType::String.to_string(), "string");
        assert_eq!(OptionType::Boolean.to_string(), "boolean");
        assert_eq!(OptionType::Size.to_string(), "size");
    }

    #[test]
    fn test_option_category_display() {
        assert_eq!(OptionCategory::General.to_string(), "general");
        assert_eq!(OptionCategory::BitTorrent.to_string(), "bittorrent");
    }

    #[test]
    fn test_option_value_variants() {
        let s = OptionValue::Str("hello".into());
        assert_eq!(s.as_str().unwrap(), "hello");

        let n = OptionValue::Int(42);
        assert_eq!(n.as_i64().unwrap(), 42);

        let b = OptionValue::Bool(true);
        assert!(b.as_bool().unwrap());

        let l = OptionValue::List(vec!["a".into(), "b".into()]);
        assert_eq!(l.as_list().unwrap().len(), 2);

        let none = OptionValue::None;
        assert!(none.is_none());
    }

    #[test]
    fn test_option_value_display() {
        assert_eq!(OptionValue::Str("test".into()).to_string(), "test");
        assert_eq!(OptionValue::Int(99).to_string(), "99");
        assert_eq!(OptionValue::Bool(true).to_string(), "true");
        assert_eq!(
            OptionValue::List(vec!["x".into(), "y".into()]).to_string(),
            "x,y"
        );
    }

    #[test]
    fn test_option_value_to_json() {
        let v = OptionValue::Str("hello".into());
        let jv: serde_json::Value = (&v).into();
        assert_eq!(jv, "hello");

        let v2 = OptionValue::Int(123);
        let jv2: serde_json::Value = (&v2).into();
        assert_eq!(jv2, 123);

        let v3 = OptionValue::Bool(false);
        let jv3: serde_json::Value = (&v3).into();
        assert_eq!(jv3, false);

        let v4 = OptionValue::List(vec!["a".into()]);
        let jv4: serde_json::Value = (&v4).into();
        assert!(jv4.is_array());
    }

    #[test]
    fn test_option_value_from_json() {
        let ov: OptionValue = serde_json::json!("test string").into();
        assert_eq!(ov.as_str().unwrap(), "test string");

        let ov2: OptionValue = serde_json::json!(42).into();
        assert_eq!(ov2.as_i64().unwrap(), 42);

        let ov3: OptionValue = serde_json::json!(true).into();
        assert!(ov3.as_bool().unwrap());

        let ov4: OptionValue = serde_json::json!(["a", "b"]).into();
        assert_eq!(ov4.as_list().unwrap().len(), 2);
    }

    #[test]
    fn test_size_parsing() {
        assert_eq!(OptionValue::parse_size_str("100"), 100);
        assert_eq!(OptionValue::parse_size_str("1K"), 1024);
        assert_eq!(OptionValue::parse_size_str("2M"), 2 * 1024 * 1024);
        assert_eq!(OptionValue::parse_size_str("1G"), 1024u64 * 1024 * 1024);
        assert_eq!(OptionValue::parse_size_str("0"), 0);
    }

    #[test]
    fn test_size_display() {
        assert!(OptionValue::to_size_string(500).contains("500"));
        assert!(OptionValue::to_size_string(2048).contains("K"));
        assert!(OptionValue::to_size_string(3 * 1024 * 1024).contains("M"));
    }

    #[test]
    fn test_option_def_builder() {
        let def = OptionDef::new("split", OptionType::Integer)
            .short('s')
            .default(OptionValue::Int(5))
            .desc("Connections per download")
            .range(1, 16)
            .category(OptionCategory::HttpFtp);
        assert_eq!(def.name(), "split");
        assert_eq!(def.short_name(), Some('s'));
        assert_eq!(def.opt_type(), OptionType::Integer);
        assert!(!def.is_deprecated());
        assert!(!def.is_hidden());
    }

    #[test]
    fn test_option_def_parse_integer() {
        let def = OptionDef::new("split", OptionType::Integer).range(1, 16);
        let v = def.parse_value("5").unwrap();
        assert_eq!(v.as_i64().unwrap(), 5);

        let err = def.parse_value("0");
        assert!(err.is_err());

        let err2 = def.parse_value("abc");
        assert!(err2.is_err());
    }

    #[test]
    fn test_option_def_parse_boolean() {
        let def = OptionDef::new("verbose", OptionType::Boolean);
        assert!(def.parse_value("true").unwrap().as_bool().unwrap());
        assert!(def.parse_value("yes").unwrap().as_bool().unwrap());
        assert!(def.parse_value("1").unwrap().as_bool().unwrap());
        assert!(!def.parse_value("false").unwrap().as_bool().unwrap());
        assert!(!def.parse_value("no").unwrap().as_bool().unwrap());
        assert!(def.parse_value("invalid").is_err());
    }

    #[test]
    fn test_option_def_parse_list() {
        let def = OptionDef::new("header", OptionType::List);
        let v = def.parse_value("X-Custom:foo,X-Bar:baz").unwrap();
        assert_eq!(v.as_list().unwrap().len(), 2);
    }

    #[test]
    fn test_option_def_parse_empty_uses_default() {
        let def = OptionDef::new("dir", OptionType::Path).default(OptionValue::Str("/tmp".into()));
        let v = def.parse_value("").unwrap();
        assert_eq!(v.as_str().unwrap(), "/tmp");
    }

    #[test]
    fn test_registry_creation() {
        let reg = OptionRegistry::new();
        assert!(reg.count() >= 60);
        assert!(reg.get("split").is_some());
        assert!(reg.get("nonexistent-option").is_none());
    }

    #[test]
    fn test_registry_by_category() {
        let reg = OptionRegistry::new();
        let general = reg.by_category(OptionCategory::General);
        let bt = reg.by_category(OptionCategory::BitTorrent);
        let rpc = reg.by_category(OptionCategory::Rpc);
        assert!(!general.is_empty());
        assert!(!bt.is_empty());
        assert!(!rpc.is_empty());
    }

    #[test]
    fn test_registry_defaults_are_valid() {
        let reg = OptionRegistry::new();
        for def in reg.all().values() {
            if !matches!(def.default_value(), OptionValue::None) {
                let parsed = def.parse_value(&def.default_value().to_string());
                assert!(
                    parsed.is_ok(),
                    "Default value for '{}' failed to re-parse: {:?}",
                    def.name(),
                    parsed.err()
                );
            }
        }
    }

    #[test]
    fn test_default_registry() {
        let reg = OptionRegistry::default();
        assert!(reg.count() > 0);
    }
}
