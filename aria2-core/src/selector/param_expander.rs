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
    /// Bracket `[START-END[:STEP]]` numeric range syntax.
    Bracket {
        start: u64,
        end: u64,
        step: u64,
        width: usize,
    },
    /// [START-END[:STEP]] alphabetic range syntax.
    AlphaBracket {
        start: String,
        end: String,
        step: u64,
        width: usize,
    },
    /// {CHOICE1,CHOICE2,...} choice expansion.
    Choice { values: Vec<String> },
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

    // Pattern 2: {choice,choice} expansion.
    let choice_re = Regex::new(r"\{([^{}]+,[^{}]+)\}").unwrap();
    for cap in choice_re.captures_iter(uri) {
        patterns.push((
            cap.get(0).unwrap().start(),
            ParamPattern::Choice {
                values: cap[1].split(',').map(str::to_owned).collect(),
            },
        ));
    }

    // Pattern 3: ${...} numeric form.
    let braced_re = Regex::new(r"\$\{([^}]+)\}").unwrap();
    for cap in braced_re.captures_iter(uri) {
        let full_match = cap.get(0).unwrap();
        let inner = cap.get(1).unwrap().as_str();

        let pattern = match parse_braced_pattern(inner) {
            Some(pattern) => pattern,
            None => continue,
        };
        patterns.push((full_match.start(), pattern));
    }

    // Pattern 4: [...] bracket form (numeric or alphabetic range syntax).
    // Need to be careful not to match IPv6 addresses or other bracket usages.
    let bracket_re = Regex::new(r"\[([A-Za-z0-9]+)-([A-Za-z0-9]+)(?::([0-9]+))?\]").unwrap();
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
        let step = step_str.and_then(|s| s.parse::<u64>().ok()).unwrap_or(1);
        if step == 0 {
            continue;
        }

        if let (Ok(start), Ok(end)) = (start_str.parse::<u64>(), end_str.parse::<u64>()) {
            patterns.push((
                full_match.start(),
                ParamPattern::Bracket {
                    start,
                    end,
                    step,
                    width: max(start_str.len(), end_str.len()),
                },
            ));
        } else if is_alpha_range(start_str, end_str) {
            patterns.push((
                full_match.start(),
                ParamPattern::AlphaBracket {
                    start: start_str.to_string(),
                    end: end_str.to_string(),
                    step,
                    width: start_str.len(),
                },
            ));
        }
    }

    // Sort by position to maintain left-to-right order.
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

fn is_alpha_range(start: &str, end: &str) -> bool {
    !start.is_empty()
        && start.len() == end.len()
        && ((start.bytes().all(|b| b.is_ascii_lowercase())
            && end.bytes().all(|b| b.is_ascii_lowercase()))
            || (start.bytes().all(|b| b.is_ascii_uppercase())
                && end.bytes().all(|b| b.is_ascii_uppercase())))
}

fn alpha_value(value: &str) -> u32 {
    value.bytes().fold(0, |acc, byte| {
        acc * 26 + u32::from(byte.to_ascii_lowercase() - b'a' + 1)
    })
}

fn alpha_string(mut value: u32, uppercase: bool, width: usize) -> String {
    let mut bytes = Vec::new();
    while value > 0 {
        value -= 1;
        bytes.push((b'a' + (value % 26) as u8) as char);
        value /= 26;
    }
    while bytes.len() < width {
        bytes.push('a');
    }
    bytes.reverse();
    let result: String = bytes.into_iter().collect();
    if uppercase {
        result.to_ascii_uppercase()
    } else {
        result
    }
}

fn generate_alpha_range(start: &str, end: &str, step: u64, width: usize) -> Vec<String> {
    if step == 0 {
        return Vec::new();
    }
    let start_value = alpha_value(start);
    let end_value = alpha_value(end);
    let uppercase = start.bytes().next().is_some_and(|b| b.is_ascii_uppercase());
    let mut value = start_value;
    let mut result = Vec::new();
    while value <= end_value {
        result.push(alpha_string(value, uppercase, width));
        value = match value.checked_add(step as u32) {
            Some(next) => next,
            None => break,
        };
    }
    result
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
        ParamPattern::AlphaBracket {
            start,
            end,
            step,
            width,
        } => generate_alpha_range(start, end, *step, *width),
        ParamPattern::Choice { values } => {
            values.iter().map(|value| value.trim().to_owned()).collect()
        }
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
/// - **Bracket `[...]`**: Numeric or alphabetic range with optional step
/// - **Choice `{a,b}`**: Expands each choice, with Cartesian product across multiple patterns
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

    let mut template = uri.to_string();
    for (index, (position, pattern)) in patterns.iter().enumerate().rev() {
        let start = *position;
        let end = start + pattern_source_len(&uri[start..], pattern);
        template.replace_range(start..end, &format!("\u{1f}{index}\u{1f}"));
    }
    cartesian_product_replace(&template, &all_expansions)
}

/// Replace marked pattern occurrences with the Cartesian product of expansions.
fn cartesian_product_replace(uri: &str, expansions: &[Vec<String>]) -> Vec<String> {
    let mut results = vec![uri.to_string()];
    for (index, expansion_set) in expansions.iter().enumerate() {
        let marker = format!("\u{1f}{index}\u{1f}");
        let mut next = Vec::with_capacity(results.len() * expansion_set.len());
        for result in &results {
            for value in expansion_set {
                next.push(result.replacen(&marker, value, 1));
            }
        }
        results = next;
    }
    results
}

fn pattern_source_len(source: &str, pattern: &ParamPattern) -> usize {
    match pattern {
        ParamPattern::Simple { .. } => {
            1 + source[1..].bytes().take_while(u8::is_ascii_digit).count()
        }
        ParamPattern::Braced { .. } | ParamPattern::Choice { .. } => {
            source.find('}').map_or(0, |end| end + 1)
        }
        ParamPattern::Bracket { .. } | ParamPattern::AlphaBracket { .. } => {
            source.find(']').map_or(0, |end| end + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("param_expander_tests.rs");
}
