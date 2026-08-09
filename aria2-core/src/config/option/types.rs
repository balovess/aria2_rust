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
    IntegerRange,
    Float,
    Boolean,
    List,
    Enum,
    IndexOut,
    Path,
    Size,
}

impl fmt::Display for OptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String => write!(f, "string"),
            Self::Integer => write!(f, "integer"),
            Self::IntegerRange => write!(f, "integer-range"),
            Self::Float => write!(f, "float"),
            Self::Boolean => write!(f, "boolean"),
            Self::List => write!(f, "list"),
            Self::Enum => write!(f, "enum"),
            Self::IndexOut => write!(f, "index-out"),
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
        Self::parse_size_str_checked(s).unwrap_or(0)
    }

    /// Parse an aria2 size value such as `128K` or `2M` without hiding
    /// malformed input behind the value zero.
    pub fn parse_size_str_checked(s: &str) -> Result<u64, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("size must not be empty".to_string());
        }
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
        if suffix == 1
            && let Ok(bytes) = num_part.parse::<u64>()
        {
            return Ok(bytes);
        }
        let number = num_part
            .parse::<f64>()
            .map_err(|_| format!("invalid size '{}'", s))?;
        if !number.is_finite() || number < 0.0 {
            return Err(format!("invalid size '{}'", s));
        }
        let bytes = number * suffix as f64;
        if bytes > u64::MAX as f64 {
            return Err(format!("size '{}' is too large", s));
        }
        Ok(bytes as u64)
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
    /// Exact wire values accepted for `OptionType::Enum`.
    ///
    /// An empty slice keeps custom definitions backward-compatible and means
    /// that the enum is open-ended until its owner supplies a choice set.
    pub allowed_values: &'static [&'static str],
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
            allowed_values: &[],
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

    pub fn allowed_values(&self) -> &[&'static str] {
        self.allowed_values
    }

    pub fn parse_value(&self, s: &str) -> Result<OptionValue, String> {
        if s.is_empty() {
            return Ok(self.default_value.clone());
        }
        match self.opt_type {
            OptionType::String | OptionType::Path => Ok(OptionValue::Str(s.to_string())),
            OptionType::IntegerRange => {
                let max = self
                    .max
                    .map(|max| max.min(i64::MAX as u64) as i64)
                    .unwrap_or(i64::MAX);
                parse_integer_segments(s, self.min.unwrap_or(i64::MIN), max)
                    .map(|_| OptionValue::Str(s.to_string()))
            }
            OptionType::Enum => {
                if !self.allowed_values.is_empty() && !self.allowed_values.contains(&s) {
                    return Err(format!(
                        "invalid choice '{}', allowed values: {}",
                        s,
                        self.allowed_values.join(", ")
                    ));
                }
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
            OptionType::IndexOut => parse_index_out(s).map(|_| OptionValue::Str(s.to_string())),
            OptionType::Size => {
                let value = OptionValue::parse_size_str_checked(s)?;
                if value > i64::MAX as u64 {
                    return Err(format!("size '{}' is too large", s));
                }
                validate_unsigned_bounds(&self.name, value, self.min, self.max)?;
                Ok(OptionValue::Int(value as i64))
            }
            OptionType::Float => {
                let value = s
                    .parse::<f64>()
                    .map_err(|e| format!("invalid float '{}': {}", s, e))?;
                if !value.is_finite() {
                    return Err(format!("invalid float '{}'", s));
                }
                if let Some(min) = self.min
                    && value < min as f64
                {
                    return Err(format!("value {} < minimum {}", value, min));
                }
                if let Some(max) = self.max
                    && value > max as f64
                {
                    return Err(format!("value {} exceeds maximum {}", value, max));
                }
                Ok(OptionValue::Float(value))
            }
            OptionType::Boolean => match s.to_lowercase().as_str() {
                "true" | "yes" | "1" | "on" => Ok(OptionValue::Bool(true)),
                "false" | "no" | "0" | "off" => Ok(OptionValue::Bool(false)),
                _ => Err(format!("invalid boolean '{}'", s)),
            },
            OptionType::List => Ok(OptionValue::List(
                s.split([',', '\n']).map(|x| x.trim().to_string()).collect(),
            )),
        }
    }
}

/// Parse aria2's comma-separated integer and inclusive-range syntax.
pub fn parse_integer_segments(
    value: &str,
    min: i64,
    max: i64,
) -> Result<Vec<std::ops::RangeInclusive<i64>>, String> {
    let mut ranges = Vec::new();
    for raw_segment in value.split(',') {
        let segment = raw_segment.trim();
        if segment.is_empty() {
            continue;
        }

        let mut endpoints = segment.split('-');
        let start = endpoints
            .next()
            .and_then(|endpoint| endpoint.trim().parse::<i64>().ok())
            .ok_or_else(|| format!("bad integer range '{}'", segment))?;
        let end = match endpoints.next() {
            Some(endpoint) if !endpoint.trim().is_empty() => endpoint
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("bad integer range '{}'", segment))?,
            Some(_) => return Err(format!("incomplete integer range '{}'", segment)),
            None => start,
        };
        if endpoints.next().is_some() || start > end {
            return Err(format!("bad integer range '{}'", segment));
        }
        if start < min || end > max {
            return Err(format!(
                "integer range '{}' must be between {} and {}",
                segment, min, max
            ));
        }
        ranges.push(start..=end);
    }

    if ranges.is_empty() {
        return Err("integer range must not be empty".to_string());
    }
    Ok(ranges)
}

/// Parse aria2's cumulative `INDEX=PATH` wire representation.
///
/// The option is stored as newline-delimited text so repeated CLI/config/RPC
/// values preserve their original ordering. Execution code should use this
/// parser rather than reimplementing the delimiter and validation rules.
pub fn parse_index_out(value: &str) -> Result<Vec<(usize, String)>, String> {
    let mut entries = Vec::new();
    for raw_line in value.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        let (index, path) = line
            .split_once('=')
            .ok_or_else(|| format!("invalid index-out value '{}'", line))?;
        let index = index
            .parse::<u32>()
            .map_err(|_| format!("invalid index-out index '{}'", index))?
            as usize;
        if path.is_empty() {
            return Err(format!("index-out path for {} must not be empty", index));
        }
        entries.push((index, path.to_string()));
    }
    Ok(entries)
}

fn validate_unsigned_bounds(
    name: &str,
    value: u64,
    min: Option<i64>,
    max: Option<u64>,
) -> Result<(), String> {
    if let Some(min) = min
        && min >= 0
        && value < min as u64
    {
        return Err(format!("{} value {} < minimum {}", name, value, min));
    }
    if let Some(max) = max
        && value > max
    {
        return Err(format!("{} value {} exceeds maximum {}", name, value, max));
    }
    Ok(())
}
