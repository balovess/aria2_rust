//! Core type definitions for configuration options.
//!
//! Provides [`OptionType`], [`OptionCategory`], [`OptionValue`], and [`OptionDef`].

use std::fmt;

// =========================================================================
// Core Type Definitions
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
        match self {
            Self::Float(v) => Some(*v),
            Self::Usize(n) => Some(*n as f64),
            Self::Int(n) => Some(*n as f64),
            _ => None,
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
    /// If set, multiple calls to `set_raw` for this option will append values
    /// separated by this delimiter rather than overwrite. Used for cumulative
    /// options like `header` (delimiter: `"\n"`) and `bt-tracker`.
    pub cumulative_delimiter: Option<&'static str>,
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
            cumulative_delimiter: None,
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
