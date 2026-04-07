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

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.source, self.option, self.message)
    }
}

pub struct ConfigParser {
    options: HashMap<String, OptionValue>,
    sources: Vec<ConfigSource>,
    errors: Vec<ConfigError>,
    registry: OptionRegistry,
}

impl ConfigParser {
    pub fn new() -> Self {
        Self { options: HashMap::new(), sources: Vec::new(), errors: Vec::new(), registry: OptionRegistry::new() }
    }

    pub fn with_registry(registry: OptionRegistry) -> Self {
        Self { registry, ..Self::new() }
    }

    pub fn set(&mut self, name: impl Into<String>, value: OptionValue) {
        let key = name.into();
        if let Some(def) = self.registry.get(&key.as_str()) {
            match def.parse_value(&value.to_string()) {
                Ok(v) => { self.options.insert(key, v); }
                Err(e) => self.errors.push(ConfigError {
                    source: ConfigSource::CommandLine,
                    option: key.clone(),
                    message: e,
                }),
            }
        } else {
            self.options.insert(key, value);
        }
    }

    pub fn set_raw(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let key = name.into();
        if let Some(def) = self.registry.get(&key.as_str()) {
            match def.parse_value(&value.into()) {
                Ok(v) => { self.options.insert(key, v); }
                Err(e) => self.errors.push(ConfigError {
                    source: ConfigSource::CommandLine,
                    option: key.clone(),
                    message: e,
                }),
            }
        } else {
            self.options.insert(key, OptionValue::Str(value.into()));
        }
    }

    pub fn get(&self, name: &str) -> Option<&OptionValue> { self.options.get(name) }
    pub fn get_str(&self, name: &str) -> Option<&str> { self.options.get(name).and_then(|v| v.as_str()) }
    pub fn get_i64(&self, name: &str) -> Option<i64> { self.options.get(name).and_then(|v| v.as_i64()) }
    pub fn get_bool(&self, name: &str) -> Option<bool> { self.options.get(name).and_then(|v| v.as_bool()) }
    pub fn contains(&self, name: &str) -> bool { self.options.contains_key(name) }

    pub fn parse_cli_args(&mut self, args: &[&str]) {
        self.sources.push(ConfigSource::CommandLine);
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg.starts_with("--") {
                let opt_name = &arg[2..];
                if opt_name.starts_with("no-") && opt_name.len() > 3 {
                    let real_name = &opt_name[3..];
                    self.set(real_name, OptionValue::Bool(false));
                } else if opt_name.contains('=') {
                    let parts: Vec<&str> = opt_name.splitn(2, '=').collect();
                    if parts.len() == 2 { self.set_raw(parts[0], parts[1]); }
                } else if i + 1 < args.len() && !args[i+1].starts_with('-') {
                            self.set_raw(opt_name, args[i+1]);
                    i += 1;
                } else {
                    if let Some(def) = self.registry.get(opt_name) {
                        if def.opt_type() == OptionType::Boolean {
                            self.set(opt_name, OptionValue::Bool(true));
                        } else {
                            self.set_raw(opt_name, "");
                        }
                    } else {
                        self.set_raw(opt_name, "");
                    }
                }
            } else if arg.starts_with('-') && arg.len() == 2 {
                let c = arg.chars().nth(1).unwrap();
                let opt_name = self.registry.all().values()
                    .find(|def| def.short_name() == Some(c))
                    .map(|def| def.name().to_string());
                if let Some(name) = opt_name {
                    if i + 1 < args.len() && !args[i+1].starts_with('-') {
                            self.set_raw(&name, args[i+1]);
                        i += 1;
                    } else {
                        self.set(&name, OptionValue::Bool(true));
                    }
                }
            } else if arg.starts_with('@') {
                self.parse_file(&arg[1..]);
            } else {
                i += 1;
            }
            i += 1;
        }
    }

    pub fn parse_file(&mut self, path: &str) {
        self.sources.push(ConfigSource::ConfigFile);
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('[') || line.starts_with(';') {
                    continue;
                }
                if let Some(eq_pos) = line.find('=') {
                    let name = line[..eq_pos].trim();
                    let value = line[eq_pos+1..].trim();
                    if !name.is_empty() { self.set_raw(name, value); }
                }
            }
        }
    }

    pub fn parse_env_vars(&mut self) {
        self.sources.push(ConfigSource::Environment);
        for (key, value) in std::env::vars() {
            if key.starts_with("ARIA2_") {
                let opt_name = key[6..].to_lowercase().replace('_', "-");
                self.set_raw(opt_name, &value);
            }
        }
    }

    pub fn apply_defaults(&mut self) {
        self.sources.push(ConfigSource::Defaults);
        for def in self.registry.all().values() {
            if !matches!(def.default_value(), OptionValue::None) {
                self.options.entry(def.name().to_string()).or_insert_with(|| def.default_value().clone());
            }
        }
    }

    pub fn load_defaults_first(&mut self) {
        self.apply_defaults();
        self.parse_env_vars();
        let conf_path = self.get_str("conf-path")
            .map(|s| s.to_string())
            .unwrap_or_else(|| String::new());
        if !conf_path.is_empty() && std::path::Path::new(&conf_path).exists() {
            self.parse_file(&conf_path);
        }
    }

    pub fn options(&self) -> &HashMap<String, OptionValue> { &self.options }
    pub fn errors(&self) -> &[ConfigError] { &self.errors }
    pub fn has_errors(&self) -> bool { !self.errors.is_empty() }
    pub fn source_count(&self) -> usize { self.sources.len() }

    pub fn to_json_map(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (k, v) in &self.options {
            map.insert(k.clone(), <&OptionValue as Into<serde_json::Value>>::into(v));
        }
        serde_json::Value::Object(map)
    }
}

impl Default for ConfigParser {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_source_display() {
        assert_eq!(ConfigSource::CommandLine.to_string(), "command-line");
        assert_eq!(ConfigSource::Defaults.to_string(), "defaults");
    }

    #[test]
    fn test_parser_creation() {
        let p = ConfigParser::new();
        assert_eq!(p.source_count(), 0);
        assert!(!p.has_errors());
    }

    #[test]
    fn test_set_and_get() {
        let mut p = ConfigParser::new();
        p.set("dir", OptionValue::Str("/downloads".into()));
        assert_eq!(p.get_str("dir").unwrap(), "/downloads");
        assert!(p.contains("dir"));
        assert!(!p.contains("nonexistent"));
    }

    #[test]
    fn test_set_raw_string() {
        let mut p = ConfigParser::new();
        p.set_raw("split", "8");
        assert_eq!(p.get_i64("split").unwrap(), 8);
    }

    #[test]
    fn test_set_raw_bool() {
        let mut p = ConfigParser::new();
        p.set_raw("check-certificate", "false");
        assert!(!p.get_bool("check-certificate").unwrap());
    }

    #[test]
    fn test_parse_cli_args_basic() {
        let mut p = ConfigParser::new();
        p.parse_cli_args(&["--dir=/tmp", "--split=3", "--quiet"]);
        assert_eq!(p.get_str("dir").unwrap(), "/tmp");
        assert_eq!(p.get_i64("split").unwrap(), 3);
        assert!(p.get_bool("quiet").unwrap());
    }

    #[test]
    fn test_parse_cli_args_no_prefix() {
        let mut p = ConfigParser::new();
        p.parse_cli_args(&["--no-check-certificate", "--no-continue"]);
        assert!(!p.get_bool("check-certificate").unwrap());
        assert!(!p.get_bool("continue").unwrap());
    }

    #[test]
    fn test_parse_cli_args_space_separated() {
        let mut p = ConfigParser::new();
        p.parse_cli_args(&["--dir", "/opt/downloads", "--out", "file.iso"]);
        assert_eq!(p.get_str("dir").unwrap(), "/opt/downloads");
        assert_eq!(p.get_str("out").unwrap(), "file.iso");
    }

    #[test]
    fn test_parse_cli_boolean_flag() {
        let mut p = ConfigParser::new();
        p.parse_cli_args(&["-q", "--dry-run"]);
        assert!(p.get_bool("quiet").unwrap());
        assert!(p.get_bool("dry-run").unwrap());
    }

    #[test]
    fn test_apply_defaults() {
        let mut p = ConfigParser::new();
        p.apply_defaults();
        assert_eq!(p.get_str("dir").unwrap(), ".");
        assert_eq!(p.get_i64("split").unwrap(), 5);
        assert!(p.get_bool("enable-color").unwrap());
    }

    #[test]
    fn test_load_order() {
        let mut p = ConfigParser::new();
        p.load_defaults_first();
        p.set("dir", OptionValue::Str("/override".into()));
        assert_eq!(p.get_str("dir").unwrap(), "/override");
    }

    #[test]
    fn test_to_json_map() {
        let mut p = ConfigParser::new();
        p.set("dir", OptionValue::Str("/tmp".into()));
        p.set("split", OptionValue::Int(10));
        let map = p.to_json_map();
        assert!(map.get("dir").is_some());
        assert!(map.get("split").is_some());
    }

    #[test]
    fn test_error_on_invalid_integer() {
        let mut p = ConfigParser::new();
        p.set_raw("split", "not_a_number");
        assert!(p.has_errors());
        assert_eq!(p.errors()[0].option, "split");
    }

    #[test]
    fn test_error_on_out_of_range() {
        let mut p = ConfigParser::new();
        p.set_raw("split", "100");
        assert!(p.has_errors());
    }

    #[test]
    fn test_error_display() {
        let err = ConfigError {
            source: ConfigSource::CommandLine,
            option: "split".into(),
            message: "value 100 exceeds maximum 16".into(),
        };
        let s = format!("{}", err);
        assert!(s.contains("split"));
        assert!(s.contains("command-line"));
    }

    #[test]
    fn test_default_parser() {
        let p = ConfigParser::default();
        assert_eq!(p.source_count(), 0);
    }
}
