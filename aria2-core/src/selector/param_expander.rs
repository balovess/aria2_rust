//! Parameterized URI expander for batch download support.
//!
//! This module implements pattern expansion for parameterized URIs, supporting:
//! - Simple positional: `$num` (starting from 1)
//! - Zero-padded brace form: `${num}`, `${start-end}`, `${start-end:step}`
//! - Range syntax: `[FROM-TO]`, `[FROM-TO:STEP]`
//! - Combined patterns (Cartesian product for multiple placeholders)

use regex::Regex;
use std::cmp::{Ordering, max};

/// Represents a single parameterized placeholder in a URI
#[derive(Debug, Clone, PartialEq)]
enum ParamPattern {
    /// Simple $N - generates 10^N values starting from 1 (no zero-padding)
    Simple { value: u64 },
    /// ${N} zero-padded or ${START-END[:STEP]}
    Braced {
        start: u64,
        end: u64,
        step: u64,
        width: usize,
    },
    /// [START-END[:STEP]] range syntax
    Bracket {
        start: u64,
        end: u64,
        step: u64,
        width: usize,
    },
}

/// Parse a URI string to detect parameterized patterns.
///
/// Returns a vector of `ParamPattern` instances found in the URI, along with their positions.
/// Patterns are returned in order of appearance from left to right.
fn find_param_patterns(uri: &str) -> Vec<(usize, ParamPattern)> {
    let mut patterns = Vec::new();

    // Pattern 1: $num (simple positional, must be followed by non-digit or end)
    // Match $ followed by one or more digits, but not if preceded by $ or {
    // Note: regex crate does not support lookbehind assertions, so we match $\d+ first
    // then filter out matches preceded by ${ or $ using manual string checks
    let simple_re = Regex::new(r"\$(\d+)").unwrap();
    for cap in simple_re.captures_iter(uri) {
        let full_match = cap.get(0).unwrap();
        let start = full_match.start();

        // Skip matches that are part of ${...} patterns (preceded by {)
        // or part of $$ escape sequences (preceded by $)
        if start > 0 {
            let prev_char = uri.as_bytes()[start - 1];
            if prev_char == b'{' || prev_char == b'$' {
                continue;
            }
        }

        let digits = cap.get(1).unwrap().as_str();
        // Parse the numeric value - $N generates 10^N values
        // e.g., $3 generates 10^3 = 1000 values: "1", "2", ..., "1000"
        let value: u64 = digits.parse().unwrap_or(1);
        patterns.push((start, ParamPattern::Simple { value }));
    }

    // Pattern 2: ${...} brace form
    // Can be: ${N}, ${START-END}, or ${START-END:STEP}
    let braced_re = Regex::new(r"\$\{([^}]+)\}").unwrap();
    for cap in braced_re.captures_iter(uri) {
        let full_match = cap.get(0).unwrap();
        let inner = cap.get(1).unwrap().as_str();

        if let Some(pattern) = parse_braced_pattern(inner) {
            patterns.push((full_match.start(), pattern));
        }
    }

    // Pattern 3: [...] bracket form (range syntax)
    // Must be [START-END] or [START-END:STEP]
    // Need to be careful not to match IPv6 addresses or other bracket usages
    let bracket_re = Regex::new(r"\[(\d+)-(\d+)(?::(\d+))?\]").unwrap();
    for cap in bracket_re.captures_iter(uri) {
        let full_match = cap.get(0).unwrap();

        // Skip if this looks like an IPv6 address (contains multiple colons before the bracket)
        let before_bracket = &uri[..full_match.start()];
        if before_bracket.ends_with(':') && before_bracket.contains("::") {
            continue; // Likely part of IPv6 address
        }

        let start_str = cap.get(1).unwrap().as_str();
        let end_str = cap.get(2).unwrap().as_str();
        let step_str = cap.get(3).map(|m| m.as_str());

        if let (Ok(start), Ok(end)) = (start_str.parse::<u64>(), end_str.parse::<u64>()) {
            let step = step_str.and_then(|s| s.parse::<u64>().ok()).unwrap_or(1);
            if step == 0 {
                continue; // Invalid step
            }

            // Determine width from the larger number's digit count
            let width = max(start_str.len(), end_str.len());

            patterns.push((
                full_match.start(),
                ParamPattern::Bracket {
                    start,
                    end,
                    step,
                    width,
                },
            ));
        }
    }

    // Sort by position to maintain left-to-right order
    patterns.sort_by_key(|(pos, _)| *pos);
    patterns
}

/// Parse the content inside ${...} braces
fn parse_braced_pattern(inner: &str) -> Option<ParamPattern> {
    // Try to parse as START-END:STEP first
    if let Some(cap) = Regex::new(r"^(\d+)-(\d+):(\d+)$").unwrap().captures(inner) {
        let start: u64 = cap[1].parse().ok()?;
        let end: u64 = cap[2].parse().ok()?;
        let step: u64 = cap[3].parse().ok()?;

        if step == 0 {
            return None;
        }

        // Width is determined by the number of digits in the first number
        let width = cap[1].len();

        return Some(ParamPattern::Braced {
            start,
            end,
            step,
            width,
        });
    }

    // Try to parse as START-END
    if let Some(cap) = Regex::new(r"^(\d+)-(\d+)$").unwrap().captures(inner) {
        let start: u64 = cap[1].parse().ok()?;
        let end: u64 = cap[2].parse().ok()?;

        // Width is determined by the number of digits in the first number
        let width = cap[1].len();

        return Some(ParamPattern::Braced {
            start,
            end,
            step: 1,
            width,
        });
    }

    // Try to parse as single number N (zero-padded)
    // Width = raw string length of the content (preserves leading zeros)
    if let Some(cap) = Regex::new(r"^(\d+)$").unwrap().captures(inner) {
        let count: u64 = cap[1].parse().ok()?;
        // Use the string length as width to preserve leading zeros (e.g., ${03} -> width 2 -> "01","02","03")
        let width = cap[1].len();

        return Some(ParamPattern::Braced {
            start: 1,
            end: count,
            step: 1,
            width,
        });
    }

    None
}

/// Format a number with zero-padding to the specified width
fn format_with_width(n: u64, width: usize) -> String {
    format!("{:0width$}", n, width = width)
}

/// Expand a single pattern into a sequence of string values
fn expand_pattern(pattern: &ParamPattern) -> Vec<String> {
    match pattern {
        ParamPattern::Simple { value } => {
            // $N generates 10^N values starting from 1, no zero-padding
            let count = 10u64.pow(*value as u32);
            (1..=count).map(|n| n.to_string()).collect()
        }
        ParamPattern::Braced {
            start,
            end,
            step,
            width,
        } => generate_range(*start, *end, *step, *width),
        ParamPattern::Bracket {
            start,
            end,
            step,
            width,
        } => generate_range(*start, *end, *step, *width),
    }
}

/// Generate a range of formatted numbers from start to end (inclusive) with given step and width
fn generate_range(start: u64, end: u64, step: u64, width: usize) -> Vec<String> {
    if step == 0 {
        return Vec::new();
    }

    let mut values = Vec::new();
    match start.cmp(&end) {
        Ordering::Less => {
            // Forward range: start <= end
            let mut current = start;
            while current <= end {
                values.push(format_with_width(current, width));
                current += step;
            }
        }
        Ordering::Greater => {
            // Reverse range: start > end
            let mut current = start;
            while current >= end {
                values.push(format_with_width(current, width));
                if current < step {
                    break; // Prevent underflow
                }
                current -= step;
            }
        }
        Ordering::Equal => {
            // Single value
            values.push(format_with_width(start, width));
        }
    }

    values
}

/// Expand a parameterized URI into concrete URIs.
///
/// This is the main entry point for URI expansion. It detects all parameterized patterns
/// in the input URI and expands them according to the following rules:
///
/// - **Simple `$num`**: Expands starting from 1, with the number of expansions determined
///   by the digit count (e.g., `$3` → 3 values: 1, 2, 3)
/// - **Braced `${...}`**: Supports ranges and zero-padding
/// - **Bracket `[...]`**: Range syntax with optional step
/// - **Multiple patterns**: Generates Cartesian product of all pattern combinations
///
/// If no patterns are detected or if parsing fails, returns a vector containing only the original URI.
pub fn expand_parameterized_uri(uri: &str) -> Vec<String> {
    let patterns = find_param_patterns(uri);

    if patterns.is_empty() {
        return vec![uri.to_string()];
    }

    // Collect all pattern expansions
    let mut all_expansions: Vec<Vec<String>> = Vec::new();

    for (_, pattern) in &patterns {
        match pattern {
            ParamPattern::Simple { value } => {
                // $N generates 10^N values starting from 1, no zero-padding
                let count = 10u64.pow(*value as u32);
                let values: Vec<String> = (1..=count).map(|n| n.to_string()).collect();
                all_expansions.push(values);
            }
            _ => {
                let values = expand_pattern(pattern);
                if values.is_empty() {
                    // If any pattern fails to expand, return original URI
                    return vec![uri.to_string()];
                }
                all_expansions.push(values);
            }
        }
    }

    if all_expansions.is_empty() {
        return vec![uri.to_string()];
    }

    // Generate Cartesian product of all expansions
    cartesian_product_replace(uri, &patterns, &all_expansions)
}

/// Replace all patterns in URI with combinations from expansions (Cartesian product)
fn cartesian_product_replace(
    uri: &str,
    _patterns: &[(usize, ParamPattern)],
    expansions: &[Vec<String>],
) -> Vec<String> {
    if expansions.is_empty() {
        return vec![uri.to_string()];
    }

    // Start with the base URI
    let mut results = vec![uri.to_string()];

    // For each expansion set, replace the corresponding pattern
    // We need to track which pattern we're replacing
    for expansion_set in expansions {
        let mut new_results = Vec::new();

        for result in &results {
            for value in expansion_set {
                // Find and replace the next unresolved pattern
                let replaced = replace_next_pattern(result, value);
                new_results.push(replaced);
            }
        }

        results = new_results;
    }

    results
}

/// Replace the first (leftmost) unresolved parameterized pattern with the given value
fn replace_next_pattern(uri: &str, replacement: &str) -> String {
    // Try to replace $N pattern first (simple)
    if let Some(pos) = find_simple_pattern_pos(uri) {
        let before = &uri[..pos];
        // Find the end of the digit sequence
        let after_start = pos + 1; // skip $
        let mut end = after_start;
        while end < uri.len() && uri.as_bytes()[end].is_ascii_digit() {
            end += 1;
        }
        let after = &uri[end..];
        return format!("{}{}{}", before, replacement, after);
    }

    // Try to replace ${...} pattern
    if let Some(pos) = uri.find("${")
        && let Some(end) = uri[pos..].find('}')
    {
        let end_pos = pos + end + 1;
        let before = &uri[..pos];
        let after = &uri[end_pos..];
        return format!("{}{}{}", before, replacement, after);
    }

    // Try to replace [...] pattern (but not IPv6)
    if let Some(pos) = find_bracket_pattern_pos(uri)
        && let Some(end) = uri[pos..].find(']')
    {
        let end_pos = pos + end + 1;
        let before = &uri[..pos];
        let after = &uri[end_pos..];
        return format!("{}{}{}", before, replacement, after);
    }

    // No pattern found, return original
    uri.to_string()
}

/// Find the position of a simple $N pattern (not preceded by {)
fn find_simple_pattern_pos(uri: &str) -> Option<usize> {
    // Note: regex crate does not support lookbehind, so we use manual filtering
    let re = Regex::new(r"\$(\d+)").unwrap();
    for m in re.find_iter(uri) {
        let start = m.start();
        if start > 0 {
            let prev_char = uri.as_bytes()[start - 1];
            if prev_char == b'{' || prev_char == b'$' {
                continue;
            }
        }
        return Some(start);
    }
    None
}

/// Find the position of a bracket pattern [N-M] that is NOT part of an IPv6 address
fn find_bracket_pattern_pos(uri: &str) -> Option<usize> {
    let re = Regex::new(r"\[(\d+-\d+(?::\d+)?)\]").unwrap();

    for m in re.find_iter(uri) {
        let before = &uri[..m.start()];
        // Skip if this looks like it could be part of an IPv6 address
        if !before.ends_with(':') || !before.contains("::") {
            return Some(m.start());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("param_expander_tests.rs");
}
