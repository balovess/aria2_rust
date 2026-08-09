//! Option validation logic.
//!
//! Provides [`parse_config_line`] which validates and extracts `(key, value)`
//! pairs from `.aria2rc` config file lines, and [`validate_option_key`] for
//! checking key validity before setting.

/// Validate a single config file line and extract the `(key, value_str)` pair.
///
/// # Returns
///
/// - `Some((key, value))` for valid `key=value` lines
/// - `None` for blank lines, comments, or invalid lines (warnings are logged)
///
/// # Validation Rules
///
/// - Blank lines and lines starting with `#` are skipped
/// - Lines must contain at least one `=` separator
/// - The key portion must not be empty after trimming
pub fn parse_config_line<'a>(
    raw_line: &'a str,
    path_display: &'a str,
    line_num: usize,
) -> Option<(&'a str, &'a str)> {
    let line = raw_line.trim();

    // Skip blank lines and comments
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    // Split on first '='
    let (key, value_str) = line.split_once('=')?;

    let key = key.trim();
    let value_str = value_str.trim();

    if key.is_empty() {
        tracing::warn!(
            path = path_display,
            line = line_num,
            "Skipping config line with empty key"
        );
        return None;
    }

    Some((key, value_str))
}

/// Validate that an option key is syntactically valid.
///
/// A valid key is non-empty and contains only lowercase alphanumeric
/// characters, hyphens, and underscores. This matches the naming
/// convention used throughout aria2's option system.
#[allow(dead_code)]
pub fn validate_option_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    key.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}
