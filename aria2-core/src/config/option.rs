//! Configuration option types, validation, and registry.
//!
//! This module provides:
//! - [`OptionType`], [`OptionCategory`], [`OptionValue`], [`OptionDef`] — core type definitions
//! - [`OptionRegistry`] — the central registry for all aria2 configuration options
//! - [`OptionValidator`], [`OptionError`], validators — dynamic validation framework
//! - [`DependencyChecker`] — inter-option dependency checking
//!
//! Built-in option registrations are in [`option_definitions`](super::option_definitions).

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde_json::Value;

// =========================================================================
// Core Type Definitions (consolidated from option_types.rs)
// =========================================================================

/// Represents the data type of a configuration option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionType {
    String,
    Integer,
    Float,
    Boolean,
    List,
    Enum,
    Path,
    Size,
}

impl fmt::Display for OptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String => write!(f, "string"),
            Self::Integer => write!(f, "integer"),
            Self::Float => write!(f, "float"),
            Self::Boolean => write!(f, "boolean"),
            Self::List => write!(f, "list"),
            Self::Enum => write!(f, "enum"),
            Self::Path => write!(f, "path"),
            Self::Size => write!(f, "size"),
        }
    }
}

/// Logical category/grouping for configuration options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionCategory {
    General,
    HttpFtp,
    BitTorrent,
    Rpc,
    Advanced,
}

impl fmt::Display for OptionCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::General => write!(f, "general"),
            Self::HttpFtp => write!(f, "http/ftp"),
            Self::BitTorrent => write!(f, "bittorrent"),
            Self::Rpc => write!(f, "rpc"),
            Self::Advanced => write!(f, "advanced"),
        }
    }
}

/// Runtime value of a configuration option.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum OptionValue {
    Bool(bool),
    Int(i64),
    Usize(usize),
    Float(f64),
    Str(String),
    List(Vec<String>),
    #[default]
    None,
}

impl fmt::Display for OptionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{}", b),
            Self::Int(n) => write!(f, "{}", n),
            Self::Usize(n) => write!(f, "{}", n),
            Self::Float(v) => write!(f, "{}", v),
            Self::Str(s) => write!(f, "{}", s),
            Self::List(items) => write!(f, "{}", items.join(",")),
            Self::None => write!(f, ""),
        }
    }
}

impl From<&OptionValue> for serde_json::Value {
    fn from(v: &OptionValue) -> Self {
        match v {
            OptionValue::Bool(b) => serde_json::json!(*b),
            OptionValue::Int(n) => serde_json::json!(*n),
            OptionValue::Usize(n) => serde_json::json!(*n),
            OptionValue::Float(v) => serde_json::json!(*v),
            OptionValue::Str(s) => serde_json::json!(s),
            OptionValue::List(items) => serde_json::json!(items),
            OptionValue::None => serde_json::Value::Null,
        }
    }
}

impl From<serde_json::Value> for OptionValue {
    fn from(val: serde_json::Value) -> Self {
        match val {
            serde_json::Value::Bool(b) => Self::Bool(b),
            serde_json::Value::Number(n) if n.is_i64() => Self::Int(n.as_i64().unwrap()),
            serde_json::Value::Number(n) if n.is_f64() => Self::Float(n.as_f64().unwrap()),
            serde_json::Value::Number(n) if n.is_u64() => Self::Usize(n.as_u64().unwrap() as usize),
            serde_json::Value::String(s) => Self::Str(s),
            serde_json::Value::Array(arr) => Self::List(
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
            ),
            _ => Self::None,
        }
    }
}

impl OptionValue {
    pub fn as_str(&self) -> Option<&str> {
        if let Self::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        if let Self::Int(n) = self {
            Some(*n)
        } else {
            None
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let Self::Float(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }
    pub fn as_list(&self) -> Option<&Vec<String>> {
        if let Self::List(l) = self {
            Some(l)
        } else {
            None
        }
    }
    pub fn as_usize(&self) -> usize {
        match self {
            Self::Usize(n) => *n,
            _ => 0,
        }
    }
    pub fn as_str_vec(&self) -> &[String] {
        match self {
            Self::List(v) => v.as_slice(),
            _ => &[],
        }
    }
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn parse_size_str(s: &str) -> u64 {
        let s = s.trim();
        let (num_part, suffix) = if s.len() > 1 {
            let last_char = s.chars().last().unwrap();
            match last_char {
                'K' | 'k' => (&s[..s.len() - 1], 1024u64),
                'M' | 'm' => (&s[..s.len() - 1], 1024 * 1024),
                'G' | 'g' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024),
                'T' | 't' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
                _ => (s, 1u64),
            }
        } else {
            (s, 1u64)
        };
        num_part
            .parse::<f64>()
            .map(|n| (n * suffix as f64) as u64)
            .unwrap_or(0)
    }

    pub fn to_size_string(bytes: u64) -> String {
        const K: u64 = 1024;
        const M: u64 = K * K;
        const G: u64 = M * K;
        const T: u64 = G * K;
        if bytes >= T {
            format!("{}T", bytes as f64 / T as f64)
        } else if bytes >= G {
            format!("{}G", bytes as f64 / G as f64)
        } else if bytes >= M {
            format!("{}M", bytes as f64 / M as f64)
        } else if bytes >= K {
            format!("{}K", bytes as f64 / K as f64)
        } else {
            format!("{}", bytes)
        }
    }
}

/// Definition/metadata for a single configuration option.
#[derive(Debug, Clone)]
pub struct OptionDef {
    pub name: String,
    pub short_name: Option<char>,
    pub opt_type: OptionType,
    pub default_value: OptionValue,
    pub description: String,
    pub category: OptionCategory,
    pub min: Option<i64>,
    pub max: Option<u64>,
    pub deprecated: bool,
    pub hidden: bool,
}

impl Default for OptionDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            short_name: None,
            opt_type: OptionType::String,
            default_value: OptionValue::None,
            description: String::new(),
            category: OptionCategory::General,
            min: None,
            max: None,
            deprecated: false,
            hidden: false,
        }
    }
}

impl OptionDef {
    pub fn new(name: impl Into<String>, opt_type: OptionType) -> Self {
        Self {
            name: name.into(),
            opt_type,
            ..Default::default()
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn short_name(&self) -> Option<char> {
        self.short_name
    }
    pub fn opt_type(&self) -> OptionType {
        self.opt_type
    }
    pub fn default_value(&self) -> &OptionValue {
        &self.default_value
    }
    pub fn get_category(&self) -> OptionCategory {
        self.category
    }
    pub fn is_deprecated(&self) -> bool {
        self.deprecated
    }
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parse_value(&self, s: &str) -> Result<OptionValue, String> {
        if s.is_empty() {
            return Ok(self.default_value.clone());
        }
        match self.opt_type {
            OptionType::String | OptionType::Path | OptionType::Enum => {
                Ok(OptionValue::Str(s.to_string()))
            }
            OptionType::Integer => s
                .parse::<i64>()
                .map(|n| {
                    if let Some(min) = self.min
                        && n < min
                    {
                        return Err(format!("value {} < minimum {}", n, min));
                    }
                    if let Some(max) = self.max
                        && (n < 0 || n as u64 > max)
                    {
                        return Err(format!("value {} exceeds maximum {}", n, max));
                    }
                    Ok(OptionValue::Int(n))
                })
                .map_err(|e| format!("invalid integer '{}': {}", s, e))?,
            OptionType::Size => Ok(OptionValue::Int(OptionValue::parse_size_str(s) as i64)),
            OptionType::Float => s
                .parse::<f64>()
                .map(OptionValue::Float)
                .map_err(|e| format!("invalid float '{}': {}", s, e)),
            OptionType::Boolean => match s.to_lowercase().as_str() {
                "true" | "yes" | "1" | "on" => Ok(OptionValue::Bool(true)),
                "false" | "no" | "0" | "off" => Ok(OptionValue::Bool(false)),
                _ => Err(format!("invalid boolean '{}'", s)),
            },
            OptionType::List => Ok(OptionValue::List(
                s.split(',').map(|x| x.trim().to_string()).collect(),
            )),
        }
    }
}

// =========================================================================
// Validation Framework (consolidated from option_validator.rs)
// =========================================================================

/// Error type for option validation failures.
#[derive(Debug, Clone)]
pub enum OptionError {
    TypeMismatch { expected: String, got: String },
    OutOfRange { value: String, min: String, max: String },
    InvalidChoice { value: String, allowed: Vec<String> },
    InvalidUrl { url: String, reason: String },
    InvalidPath { path: String, reason: String },
    PatternMismatch { value: String, pattern: String },
    DependencyConflict { option: String, conflicts_with: String },
    MissingDependency { option: String, requires: String },
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeMismatch { expected, got } => {
                write!(
                    f,
                    "type mismatch for option: expected '{}', got '{}'",
                    expected, got
                )
            }
            Self::OutOfRange { value, min, max } => {
                write!(f, "value '{}' is out of range [{}..{}]", value, min, max)
            }
            Self::InvalidChoice { value, allowed } => {
                write!(
                    f,
                    "invalid choice '{}', allowed values: {}",
                    value,
                    allowed.join(", ")
                )
            }
            Self::InvalidUrl { url, reason } => {
                write!(f, "invalid URL '{}': {}", url, reason)
            }
            Self::InvalidPath { path, reason } => {
                write!(f, "invalid path '{}': {}", path, reason)
            }
            Self::PatternMismatch { value, pattern } => {
                write!(f, "value '{}' does not match pattern '{}'", value, pattern)
            }
            Self::DependencyConflict {
                option,
                conflicts_with,
            } => {
                write!(f, "option '{}' conflicts with '{}'", option, conflicts_with)
            }
            Self::MissingDependency { option, requires } => {
                write!(f, "option '{}' requires '{}' to be set", option, requires)
            }
        }
    }
}

impl std::error::Error for OptionError {}

/// Trait for validating configuration option values.
pub trait OptionValidator: Send + Sync {
    fn validate(&self, name: &str, value: &Value) -> Result<(), OptionError>;
    fn description(&self) -> &str;
}

/// Validates that numeric values fall within a specified range.
#[derive(Debug, Clone)]
pub struct RangeValidator<T> {
    min: T,
    max: T,
}

impl<T> RangeValidator<T>
where
    T: PartialOrd + fmt::Display + Clone + 'static,
{
    pub fn new(min: T, max: T) -> Self {
        Self { min, max }
    }
}

impl OptionValidator for RangeValidator<i64> {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_i64() {
            Some(n) if n >= self.min && n <= self.max => Ok(()),
            Some(n) => Err(OptionError::OutOfRange {
                value: n.to_string(),
                min: self.min.to_string(),
                max: self.max.to_string(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "integer".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "range validator (inclusive bounds)"
    }
}

impl OptionValidator for RangeValidator<f64> {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_f64() {
            Some(v) if v >= self.min && v <= self.max => Ok(()),
            Some(v) => Err(OptionError::OutOfRange {
                value: format!("{}", v),
                min: format!("{}", self.min),
                max: format!("{}", self.max),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "float".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "range validator for floating-point numbers"
    }
}

impl OptionValidator for RangeValidator<u64> {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_u64() {
            Some(n) if n >= self.min && n <= self.max => Ok(()),
            Some(n) => Err(OptionError::OutOfRange {
                value: n.to_string(),
                min: self.min.to_string(),
                max: self.max.to_string(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "unsigned integer".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "range validator for unsigned integers"
    }
}

/// Validates that string values are in a predefined whitelist of choices.
#[derive(Debug, Clone)]
pub struct ChoiceValidator {
    allowed: Vec<String>,
}

impl ChoiceValidator {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }

    pub fn allowed_values(&self) -> &[String] {
        &self.allowed
    }
}

impl OptionValidator for ChoiceValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(s) if self.allowed.iter().any(|a| a == s) => Ok(()),
            Some(s) => Err(OptionError::InvalidChoice {
                value: s.to_string(),
                allowed: self.allowed.clone(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "string".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "choice validator (enum whitelist)"
    }
}

/// Validates URL strings for proper format and supported schemes.
#[derive(Debug, Clone, Copy)]
pub struct UrlValidator;

impl UrlValidator {
    pub fn new() -> Self {
        Self
    }

    fn is_valid_url(url: &str) -> Result<(), String> {
        if url.is_empty() {
            return Err("URL is empty".to_string());
        }

        if !url.contains("://") {
            return Err(format!("missing scheme separator in '{}'", url));
        }

        let scheme = url.split("://").next().unwrap_or("");
        if scheme.is_empty() {
            return Err("scheme is empty".to_string());
        }

        if !scheme
            .chars()
            .all(|c| c.is_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return Err(format!("invalid scheme '{}'", scheme));
        }

        let after_scheme = url.split_once("://").map(|x| x.1).unwrap_or("");
        if after_scheme.is_empty() {
            return Err("no host/path after scheme".to_string());
        }

        Ok(())
    }
}

impl Default for UrlValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl OptionValidator for UrlValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(url) => Self::is_valid_url(url).map_err(|reason| OptionError::InvalidUrl {
                url: url.to_string(),
                reason,
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "string (URL)".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "URL format validator"
    }
}

/// Validates file system paths for existence and writability.
#[derive(Debug, Clone)]
pub struct PathValidator {
    must_exist: bool,
    writable: bool,
}

impl PathValidator {
    pub fn new(must_exist: bool, writable: bool) -> Self {
        Self {
            must_exist,
            writable,
        }
    }
}

impl OptionValidator for PathValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(path_str) => {
                let path = Path::new(path_str);

                if self.must_exist && !path.exists() {
                    return Err(OptionError::InvalidPath {
                        path: path_str.to_string(),
                        reason: "path does not exist".to_string(),
                    });
                }

                if self.writable {
                    let check_path = if path.exists() {
                        path.as_os_str().to_owned()
                    } else {
                        match path.parent() {
                            Some(parent) if !parent.as_os_str().is_empty() => {
                                parent.as_os_str().to_owned()
                            }
                            _ => path.as_os_str().to_owned(),
                        }
                    };

                    if let Some(check_path_str) = check_path.to_str() {
                        let check_path = Path::new(check_path_str);
                        if check_path.exists() {
                            match std::fs::metadata(check_path) {
                                Ok(meta) => {
                                    #[cfg(unix)]
                                    {
                                        use std::os::unix::fs::PermissionsExt;
                                        let mode = meta.permissions().mode();
                                        let user_writable = mode & 0o200 != 0;
                                        if !user_writable && meta.is_dir() {
                                            return Err(OptionError::InvalidPath {
                                                path: path_str.to_string(),
                                                reason: "path is not writable".to_string(),
                                            });
                                        }
                                    }
                                    #[cfg(not(unix))]
                                    {
                                        let _ = &meta;
                                    }
                                }
                                Err(e) => {
                                    return Err(OptionError::InvalidPath {
                                        path: path_str.to_string(),
                                        reason: format!("cannot access path: {}", e),
                                    });
                                }
                            }
                        }
                    }
                }

                Ok(())
            }
            None => Err(OptionError::TypeMismatch {
                expected: "string (path)".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "file system path validator"
    }
}

/// Validates string values against a custom regular expression pattern.
#[derive(Debug, Clone)]
pub struct RegexValidator {
    pattern: String,
    compiled: regex::Regex,
}

impl RegexValidator {
    pub fn new(pattern: &str) -> Self {
        let compiled = regex::Regex::new(pattern).expect("Invalid regex pattern in RegexValidator");
        Self {
            pattern: pattern.to_string(),
            compiled,
        }
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

impl OptionValidator for RegexValidator {
    fn validate(&self, _name: &str, value: &Value) -> Result<(), OptionError> {
        match value.as_str() {
            Some(s) if self.compiled.is_match(s) => Ok(()),
            Some(s) => Err(OptionError::PatternMismatch {
                value: s.to_string(),
                pattern: self.pattern.clone(),
            }),
            None => Err(OptionError::TypeMismatch {
                expected: "string".to_string(),
                got: format!("{:?}", value),
            }),
        }
    }

    fn description(&self) -> &str {
        "custom regex pattern validator"
    }
}

/// Extended definition for a configuration option with dynamic validation support.
pub struct OptionDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub default_value: Value,
    pub validator: Option<Box<dyn OptionValidator>>,
}

impl OptionDefinition {
    pub fn new(name: &'static str, description: &'static str, default_value: Value) -> Self {
        Self {
            name,
            description,
            default_value,
            validator: None,
        }
    }

    pub fn with_validator(mut self, validator: Box<dyn OptionValidator>) -> Self {
        self.validator = Some(validator);
        self
    }

    pub fn validate(&self, value: &Value) -> Result<(), OptionError> {
        match &self.validator {
            Some(validator) => validator.validate(self.name, value),
            None => Ok(()),
        }
    }

    pub fn get_default_or_fallback<'a>(&'a self, fallback: &'a Value) -> &'a Value {
        match &self.default_value {
            Value::Null => fallback,
            other => other,
        }
    }
}

/// Checks dependencies between configuration options.
pub struct DependencyChecker {
    mutual_exclusions: Vec<(String, String)>,
    requirements: Vec<(String, String)>,
}

impl DependencyChecker {
    pub fn new() -> Self {
        Self {
            mutual_exclusions: Vec::new(),
            requirements: Vec::new(),
        }
    }

    pub fn add_mutual_exclusion(&mut self, opt_a: String, opt_b: String) {
        self.mutual_exclusions.push((opt_a, opt_b));
    }

    pub fn add_requirement(&mut self, option: String, requires: String) {
        self.requirements.push((option, requires));
    }

    pub fn check(&self, options: &HashMap<String, Value>) -> Vec<OptionError> {
        let mut errors = Vec::new();

        for (opt_a, opt_b) in &self.mutual_exclusions {
            let a_set = options.get(opt_a).is_some_and(|v| !v.is_null());
            let b_set = options.get(opt_b).is_some_and(|v| !v.is_null());

            if a_set && b_set {
                errors.push(OptionError::DependencyConflict {
                    option: opt_a.clone(),
                    conflicts_with: opt_b.clone(),
                });
            }
        }

        for (option, requires) in &self.requirements {
            let option_set = options.get(option).is_some_and(|v| !v.is_null());
            let required_set = options.get(requires).is_some_and(|v| !v.is_null());

            if option_set && !required_set {
                errors.push(OptionError::MissingDependency {
                    option: option.clone(),
                    requires: requires.clone(),
                });
            }
        }

        errors
    }

    pub fn mutual_exclusion_count(&self) -> usize {
        self.mutual_exclusions.len()
    }

    pub fn requirement_count(&self) -> usize {
        self.requirements.len()
    }
}

impl Default for DependencyChecker {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Option Registry
// =========================================================================

/// Registry of all known configuration options.
#[derive(Clone)]
pub struct OptionRegistry {
    options: HashMap<String, OptionDef>,
}

impl OptionRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            options: HashMap::new(),
        };
        reg.register_general_options();
        reg.register_http_ftp_options();
        reg.register_bt_options();
        reg.register_rpc_options();
        reg.register_advanced_options();
        reg
    }

    pub fn register(&mut self, def: OptionDef) {
        self.options.insert(def.name().to_string(), def);
    }

    pub fn get(&self, name: &str) -> Option<&OptionDef> {
        self.options.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    pub fn all(&self) -> &HashMap<String, OptionDef> {
        &self.options
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
        let def = OptionDef {
            name: "split".into(),
            opt_type: OptionType::Integer,
            short_name: Some('s'),
            default_value: OptionValue::Int(5),
            description: "Connections per download".into(),
            min: Some(1),
            max: Some(16),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        };
        assert_eq!(def.name(), "split");
        assert_eq!(def.short_name(), Some('s'));
        assert_eq!(def.opt_type(), OptionType::Integer);
        assert!(!def.is_deprecated());
        assert!(!def.is_hidden());
    }

    #[test]
    fn test_option_def_parse_integer() {
        let def = OptionDef {
            name: "split".into(),
            opt_type: OptionType::Integer,
            min: Some(1),
            max: Some(16),
            ..Default::default()
        };
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
        let def = OptionDef {
            name: "dir".into(),
            opt_type: OptionType::Path,
            default_value: OptionValue::Str("/tmp".into()),
            ..Default::default()
        };
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

    // ==================== Validator Tests ====================

    #[test]
    fn test_range_validator_in_range() {
        let validator = RangeValidator::<i64>::new(1, 16);
        assert!(validator.validate("split", &Value::from(1)).is_ok());
        assert!(validator.validate("split", &Value::from(8)).is_ok());
        assert!(validator.validate("split", &Value::from(16)).is_ok());

        let float_validator = RangeValidator::<f64>::new(0.0, 1.0);
        assert!(float_validator.validate("ratio", &Value::from(0.5)).is_ok());

        let u64_validator = RangeValidator::<u64>::new(1024, 1024 * 1024);
        assert!(u64_validator.validate("size", &Value::from(4096u64)).is_ok());
    }

    #[test]
    fn test_range_validator_out_of_range() {
        let validator = RangeValidator::<i64>::new(1, 16);
        let result = validator.validate("split", &Value::from(0));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::OutOfRange { value, min, max } => {
                assert_eq!(value, "0");
                assert_eq!(min, "1");
                assert_eq!(max, "16");
            }
            other => panic!("Expected OutOfRange error, got {:?}", other),
        }
    }

    #[test]
    fn test_choice_validator_enum() {
        let validator = ChoiceValidator::new(vec![
            "debug".to_string(),
            "info".to_string(),
            "warn".to_string(),
            "error".to_string(),
        ]);
        assert!(validator.validate("log-level", &Value::String("debug".into())).is_ok());
        assert!(validator.validate("log-level", &Value::String("verbose".into())).is_err());
    }

    #[test]
    fn test_url_validator_malformed() {
        let validator = UrlValidator::new();
        assert!(validator.validate("tracker", &Value::String("http://example.com:6969/announce".into())).is_ok());
        assert!(validator.validate("url", &Value::String("not-a-url".into())).is_err());
    }

    #[test]
    fn test_regex_validator_pattern_match() {
        let validator = RegexValidator::new(r"^[a-zA-Z0-9.-]+:\d+$");
        assert!(validator.validate("proxy", &Value::String("proxy.example.com:8080".into())).is_ok());
        assert!(validator.validate("proxy", &Value::String("not-valid".into())).is_err());
    }

    #[test]
    fn test_dependency_checker() {
        let mut checker = DependencyChecker::new();
        checker.add_mutual_exclusion("ftp-pasv".to_string(), "ftp-port".to_string());
        checker.add_requirement("bt-enable-lpd".to_string(), "enable-dht".to_string());

        let mut opts = HashMap::new();
        opts.insert("ftp-pasv".to_string(), Value::Bool(true));
        opts.insert("ftp-port".to_string(), Value::from(8021));
        let errors = checker.check(&opts);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn test_option_definition_validation() {
        let def = OptionDefinition {
            name: "max-connections",
            description: "Maximum connections per server",
            default_value: Value::from(16),
            validator: Some(Box::new(RangeValidator::<i64>::new(1, 32))),
        };
        assert!(def.validate(&Value::from(8)).is_ok());
        assert!(def.validate(&Value::from(0)).is_err());
    }

    #[test]
    fn test_option_error_display() {
        let err = OptionError::TypeMismatch {
            expected: "integer".to_string(),
            got: "string".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("type mismatch"));
    }
}
