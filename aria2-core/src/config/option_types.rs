//! Type definitions for aria2 configuration options.
//!
//! This module contains all core data types used to represent configuration
//! options in aria2-rust:
//!
//! - [`OptionType`] - The data type of an option (string, integer, boolean, etc.)
//! - [`OptionCategory`] - The logical grouping/category of an option
//! - [`OptionValue`] - A runtime value that can hold any supported type
//! - [`OptionDef`] - Metadata and validation rules for a single option

use std::fmt;

/// Represents the data type of a configuration option.
///
/// Each option in aria2 has a specific type that determines how its value is
/// parsed, validated, and displayed. For example, `split` is an `Integer`,
/// while `dir` is a `Path`.
///
/// # Variants
///
/// - `String` - Plain text value (e.g., `user-agent`)
/// - `Integer` - Signed 64-bit integer (e.g., `split`, `max-tries`)
/// - `Float` - Floating-point number (e.g., `seed-ratio`)
/// - `Boolean` - True/false flag (e.g., `quiet`, `continue`)
/// - `List` - Comma-separated values (e.g., `header`, `no-proxy`)
/// - `Enum` - One of a predefined set of strings (e.g., `log-level`)
/// - `Path` - File system path (e.g., `dir`, `log`)
/// - `Size` - Human-readable size with K/M/G/T suffixes (e.g., `min-split-size`)
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
///
/// Options are organized into categories to make the large set of ~95 options
/// more navigable. Each category corresponds to a functional area of aria2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionCategory {
    /// General settings like directory, output filename, logging, and UI behavior.
    General,
    /// HTTP/FTP download settings including proxies, headers, timeouts, and connection management.
    HttpFtp,
    /// BitTorrent-specific settings including seeding, DHT, PEX, and peer management.
    BitTorrent,
    /// JSON-RPC/XML-RPC server settings including authentication and CORS.
    Rpc,
    /// Advanced performance tuning including bandwidth limits, disk cache, and file allocation.
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
///
/// This enum can hold any of the types defined in [`OptionType`]. It provides
/// type-safe accessors (`as_str()`, `as_i64()`, etc.) and supports conversion
/// to/from JSON for RPC serialization.
///
/// # Example
///
/// ```rust
/// use aria2_core::config::OptionValue;
///
/// let val = OptionValue::Int(42);
/// assert_eq!(val.as_i64(), Some(42));
/// assert_eq!(val.to_string(), "42");
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
pub enum OptionValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
    #[default]
    None,
}

impl fmt::Display for OptionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Str(s) => write!(f, "{}", s),
            Self::Int(n) => write!(f, "{}", n),
            Self::Float(v) => write!(f, "{}", v),
            Self::Bool(b) => write!(f, "{}", b),
            Self::List(items) => write!(f, "{}", items.join(",")),
            Self::None => write!(f, ""),
        }
    }
}

impl From<&OptionValue> for serde_json::Value {
    fn from(v: &OptionValue) -> Self {
        match v {
            OptionValue::Str(s) => serde_json::json!(s),
            OptionValue::Int(n) => serde_json::json!(*n),
            OptionValue::Float(v) => serde_json::json!(*v),
            OptionValue::Bool(b) => serde_json::json!(*b),
            OptionValue::List(items) => serde_json::json!(items),
            OptionValue::None => serde_json::Value::Null,
        }
    }
}

impl From<serde_json::Value> for OptionValue {
    fn from(val: serde_json::Value) -> Self {
        match val {
            serde_json::Value::String(s) => Self::Str(s),
            serde_json::Value::Number(n) if n.is_i64() => Self::Int(n.as_i64().unwrap()),
            serde_json::Value::Number(n) if n.is_f64() => Self::Float(n.as_f64().unwrap()),
            serde_json::Value::Bool(b) => Self::Bool(b),
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
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Parse a human-readable size string into bytes.
    ///
    /// Supports suffixes: K/k (KiB), M/m (MiB), G/g (GiB), T/t (TiB).
    /// If no suffix is present, the value is treated as raw bytes.
    ///
    /// # Example
    ///
    /// ```rust
    /// use aria2_core::config::OptionValue;
    /// assert_eq!(OptionValue::parse_size_str("1K"), 1024);
    /// assert_eq!(OptionValue::parse_size_str("2M"), 2 * 1024 * 1024);
    /// ```
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

    /// Convert a byte count to a human-readable size string.
    ///
    /// Automatically selects the appropriate suffix (K/M/G/T) based on magnitude.
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
///
/// `OptionDef` describes everything about an option except its current runtime value:
/// - Name and optional short name (e.g., `-d` for `--dir`)
/// - Data type ([`OptionType`])
/// - Default value
/// - Human-readable description
/// - Category for grouping
/// - Valid range (for numeric types)
/// - Depreciation and visibility flags
///
/// Uses the builder pattern for ergonomic construction:
///
/// ```rust
/// use aria2_core::config::{OptionDef, OptionType, OptionCategory, OptionValue};
///
/// let def = OptionDef::new("split", OptionType::Integer)
///     .short('s')
///     .default(OptionValue::Int(5))
///     .desc("Connections per download")
///     .range(1, 16)
///     .category(OptionCategory::HttpFtp);
/// ```
#[derive(Debug, Clone)]
pub struct OptionDef {
    name: String,
    short_name: Option<char>,
    opt_type: OptionType,
    default_value: OptionValue,
    description: String,
    category: OptionCategory,
    min: Option<i64>,
    max: Option<u64>,
    deprecated: bool,
    hidden: bool,
}

impl OptionDef {
    /// Create a new option definition with the given name and type.
    ///
    /// All other fields use sensible defaults: no short name, no default value,
    /// empty description, `General` category, no range limits, not deprecated or hidden.
    pub fn new(name: impl Into<String>, opt_type: OptionType) -> Self {
        Self {
            name: name.into(),
            short_name: None,
            opt_type,
            default_value: OptionValue::None,
            description: String::new(),
            category: OptionCategory::General,
            min: None,
            max: None,
            deprecated: false,
            hidden: false,
        }
    }

    // --- Builder methods ---

    pub fn short(mut self, c: char) -> Self {
        self.short_name = Some(c);
        self
    }
    pub fn default(mut self, v: impl Into<OptionValue>) -> Self {
        self.default_value = v.into();
        self
    }
    pub fn desc(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }
    pub fn category(mut self, c: OptionCategory) -> Self {
        self.category = c;
        self
    }
    pub fn range(mut self, min: i64, max: u64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }
    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }

    // --- Accessor methods ---

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

    /// Parse a string value according to this option's type and constraints.
    ///
    /// Returns an error if:
    /// - The string cannot be parsed as the expected type
    /// - The value is outside the configured range (for integers)
    /// - An invalid boolean literal is provided
    ///
    /// An empty string returns the default value (if one is set).
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
