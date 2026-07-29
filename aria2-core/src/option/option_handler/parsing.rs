//! Option value parsing and type detection.
//!
//! Provides [`detect_value_type`] for auto-detecting the type of a raw string
//! value and wrapping it in [`OptionValue`], and [`parse_kv_arg`] for parsing
//! `--key=value` / `--key:value` CLI argument patterns.

use crate::config::option::OptionValue;

/// Parse a `--key=value` or `--key:value` argument into `(key, value)`.
///
/// Returns `None` if the argument does not match either pattern.
pub fn parse_kv_arg(arg: &str) -> Option<(&str, &str)> {
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
/// - `"true"` / `"false"` / `"yes"` / `"no"` / `"on"` / `"off"` -> [`OptionValue::Bool`]
/// - Numeric string without `.` -> [`OptionValue::Usize`] (or [`OptionValue::Int`] if negative)
/// - Numeric string with `.` -> [`OptionValue::Float`]
/// - `[...]` bracket notation -> [`OptionValue::List`]
/// - Quoted string -> [`OptionValue::Str`] (quotes stripped)
/// - Anything else -> [`OptionValue::Str`]
/// - Empty string -> [`OptionValue::None`]
pub fn detect_value_type(value: &str) -> Option<OptionValue> {
    let trimmed = value.trim();

    // Empty string -> None
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
        return Some(OptionValue::List(items));
    }

    // Quoted string: "value" or 'value'
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return Some(OptionValue::Str(trimmed[1..trimmed.len() - 1].to_string()));
    }

    // Negative integer
    if let Some(neg) = trimmed.strip_prefix('-') {
        if neg.parse::<i64>().is_ok() {
            return Some(OptionValue::Int(-neg.parse::<i64>().unwrap()));
        }
    }

    // Unsigned integer
    if trimmed.parse::<usize>().is_ok() {
        return Some(OptionValue::Usize(trimmed.parse::<usize>().unwrap()));
    }

    // Float
    if trimmed.parse::<f64>().is_ok() {
        return Some(OptionValue::Float(trimmed.parse::<f64>().unwrap()));
    }

    // Default: plain string
    Some(OptionValue::Str(trimmed.to_string()))
}
