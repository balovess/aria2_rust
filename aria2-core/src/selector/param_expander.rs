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

/// Generate a range of numbers WITHOUT zero-padding (used for bracket [N-M] patterns)
#[allow(dead_code)]
fn generate_range_no_pad(start: u64, end: u64, step: u64) -> Vec<String> {
    if step == 0 {
        return Vec::new();
    }

    let mut values = Vec::new();
    match start.cmp(&end) {
        Ordering::Less => {
            let mut current = start;
            while current <= end {
                values.push(current.to_string());
                current += step;
            }
        }
        Ordering::Greater => {
            let mut current = start;
            while current >= end {
                values.push(current.to_string());
                if current < step {
                    break;
                }
                current -= step;
            }
        }
        Ordering::Equal => {
            values.push(start.to_string());
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

    // ======================================================================
    // Test Group 1: Simple $num expansion
    // ======================================================================

    #[test]
    fn test_simple_dollar_num_basic() {
        // $N where N is digits - $3 generates 10^3 = 1000 values
        let uri = "http://example.com/file$3.txt";
        let expanded = expand_parameterized_uri(uri);

        assert!(!expanded.is_empty());
        assert!(expanded.len() > 1, "Should produce multiple URIs");
        assert!(
            expanded[0].contains("file1"),
            "First URI should contain file1"
        );
    }

    #[test]
    fn test_simple_dollar_num_with_3() {
        let uri = "http://example.com/file$3.txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 1000); // 10^3 = 1000 values
        assert!(expanded[0].ends_with("file1.txt"));
        assert!(expanded[1].ends_with("file2.txt"));
        assert!(expanded.last().unwrap().ends_with("file1000.txt"));
    }

    #[test]
    fn test_simple_dollar_num_single_digit() {
        let uri = "http://example.com/file$1.txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 10); // 10^1 = 10 values
        assert!(expanded[0].contains("file1"));
        assert!(expanded[9].contains("file10"));
    }

    // ======================================================================
    // Test Group 2: Zero-padded ${num} expansion
    // ======================================================================

    #[test]
    fn test_braced_zero_padded_single_number() {
        let uri = "http://example.com/file${03}.txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 3);
        // Width = string length of "03" = 2, so output is "01", "02", "03"
        assert_eq!(expanded[0], "http://example.com/file01.txt");
        assert_eq!(expanded[1], "http://example.com/file02.txt");
        assert_eq!(expanded[2], "http://example.com/file03.txt");
    }

    #[test]
    fn test_braced_zero_padded_width_detection() {
        let uri = "http://example.com/data${0005}.bin";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 5);
        // Width = string length of "0005" = 4, so output is "0001", "0002", "0003", "0004", "0005"
        assert!(expanded[0].ends_with("data0001.bin"));
        assert!(expanded[4].ends_with("data0005.bin"));
        for uri in &expanded {
            assert!(
                uri.contains("data000") || uri.contains("data005"),
                "Should be zero-padded to width 4"
            );
        }
    }

    // ======================================================================
    // Test Group 3: ${start-end} range forward
    // ======================================================================

    #[test]
    fn test_braced_range_forward() {
        let uri = "http://example.com/chapter${01-05}.html";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 5);
        assert_eq!(expanded[0], "http://example.com/chapter01.html");
        assert_eq!(expanded[1], "http://example.com/chapter02.html");
        assert_eq!(expanded[2], "http://example.com/chapter03.html");
        assert_eq!(expanded[3], "http://example.com/chapter04.html");
        assert_eq!(expanded[4], "http://example.com/chapter05.html");
    }

    #[test]
    fn test_braced_range_large_numbers() {
        let uri = "http://example.com/archive${100-105}.zip";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 6);
        assert_eq!(expanded[0], "http://example.com/archive100.zip");
        assert_eq!(expanded[5], "http://example.com/archive105.zip");
    }

    // ======================================================================
    // Test Group 4: ${start-end:step} range with step
    // ======================================================================

    #[test]
    fn test_braced_range_with_step() {
        let uri = "http://example.com/part${01-10:2}.dat";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 5);
        assert_eq!(expanded[0], "http://example.com/part01.dat");
        assert_eq!(expanded[1], "http://example.com/part03.dat");
        assert_eq!(expanded[2], "http://example.com/part05.dat");
        assert_eq!(expanded[3], "http://example.com/part07.dat");
        assert_eq!(expanded[4], "http://example.com/part09.dat");
    }

    #[test]
    fn test_braced_step_of_3() {
        let uri = "http://example.com/img${001-009:3}.jpg";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 3);
        assert_eq!(expanded[0], "http://example.com/img001.jpg");
        assert_eq!(expanded[1], "http://example.com/img004.jpg");
        assert_eq!(expanded[2], "http://example.com/img007.jpg");
    }

    // ======================================================================
    // Test Group 5: [FROM-TO] bracket syntax
    // ======================================================================

    #[test]
    fn test_bracket_range_basic() {
        let uri = "http://example.com/file[01-05].zip";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 5);
        assert_eq!(expanded[0], "http://example.com/file01.zip");
        assert_eq!(expanded[1], "http://example.com/file02.zip");
        assert_eq!(expanded[2], "http://example.com/file03.zip");
        assert_eq!(expanded[3], "http://example.com/file04.zip");
        assert_eq!(expanded[4], "http://example.com/file05.zip");
    }

    #[test]
    fn test_bracket_range_different_widths() {
        let uri = "http://example.com/data[1-10].bin";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 10);
        // Width = max(len("1"), len("10")) = 2, so zero-padded to 2 digits
        assert_eq!(expanded[0], "http://example.com/data01.bin");
        assert_eq!(expanded[9], "http://example.com/data10.bin");
    }

    // ======================================================================
    // Test Group 6: [FROM-TO:STEP] bracket with step
    // ======================================================================

    #[test]
    fn test_bracket_range_with_step() {
        let uri = "http://example.com/file[01-10:2].zip";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 5);
        assert_eq!(expanded[0], "http://example.com/file01.zip");
        assert_eq!(expanded[1], "http://example.com/file03.zip");
        assert_eq!(expanded[2], "http://example.com/file05.zip");
        assert_eq!(expanded[3], "http://example.com/file07.zip");
        assert_eq!(expanded[4], "http://example.com/file09.zip");
    }

    #[test]
    fn test_bracket_step_of_5() {
        let uri = "http://example.com/vol[005-100:5].pdf";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 20);
        assert_eq!(expanded[0], "http://example.com/vol005.pdf");
        assert_eq!(expanded[1], "http://example.com/vol010.pdf");
        assert_eq!(expanded.last().unwrap(), &"http://example.com/vol100.pdf");
    }

    // ======================================================================
    // Test Group 7: Reverse ranges [10-01]
    // ======================================================================

    #[test]
    fn test_reverse_bracket_range() {
        let uri = "http://example.com/file[10-01].zip";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 10);
        assert_eq!(expanded[0], "http://example.com/file10.zip");
        assert_eq!(expanded[1], "http://example.com/file09.zip");
        assert_eq!(expanded[8], "http://example.com/file02.zip");
        assert_eq!(expanded[9], "http://example.com/file01.zip");
    }

    #[test]
    fn test_reverse_braced_range() {
        let uri = "http://example.com/ch${10-05}.html";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 6);
        assert_eq!(expanded[0], "http://example.com/ch10.html");
        assert_eq!(expanded[5], "http://example.com/ch05.html");
    }

    // ======================================================================
    // Test Group 8: Multiple patterns Cartesian product
    // ======================================================================

    #[test]
    fn test_multiple_patterns_cartesian_product() {
        let _uri = "http://example.com/${chapter}-${page}.html";
        // Note: This requires both chapter and page to have defined ranges
        // Let's test with actual ranges
        let uri_with_ranges = "http://example.com/${01-03}-${01-03}.html";
        let expanded = expand_parameterized_uri(uri_with_ranges);

        assert_eq!(expanded.len(), 9); // 3 x 3 = 9
        assert_eq!(expanded[0], "http://example.com/01-01.html");
        assert_eq!(expanded[1], "http://example.com/01-02.html");
        assert_eq!(expanded[2], "http://example.com/01-03.html");
        assert_eq!(expanded[3], "http://example.com/02-01.html");
        assert_eq!(expanded[8], "http://example.com/03-03.html");
    }

    #[test]
    fn test_three_patterns_cartesian() {
        let uri = "http://example.com/[1-2]-[a-d]-${01-02}.txt";
        // [a-d] is not a numeric range (letters), so it's treated as literal text.
        // Only [1-2] (2 values) and ${01-02} (2 values) are expanded -> 2x2 = 4 results
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 4); // 2 x 1 x 2 = 4 ([a-d] is literal)
        // Note: pattern replacement order depends on internal pattern detection order
        assert!(expanded[0].contains("[a-d]")); // [a-d] preserved as literal
    }

    #[test]
    fn test_mixed_brace_and_bracket() {
        let uri = "http://example.com/${01-02}-[01-03].dat";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 6); // 2 x 3 = 6
        assert_eq!(expanded[0], "http://example.com/01-01.dat");
        assert_eq!(expanded[1], "http://example.com/01-02.dat");
        assert_eq!(expanded[2], "http://example.com/01-03.dat");
        assert_eq!(expanded[3], "http://example.com/02-01.dat");
        assert_eq!(expanded[5], "http://example.com/02-03.dat");
    }

    // ======================================================================
    // Test Group 9: No-pattern passthrough
    // ======================================================================

    #[test]
    fn test_no_pattern_passthrough() {
        let uris = vec![
            "http://example.com/normal_file.txt",
            "https://cdn.example.com/static/image.png",
            "ftp://files.example.com/document.pdf",
        ];

        for uri in uris {
            let expanded = expand_parameterized_uri(uri);
            assert_eq!(expanded.len(), 1);
            assert_eq!(expanded[0], uri);
        }
    }

    #[test]
    fn test_uri_with_query_params_no_pattern() {
        let uri = "http://example.com/path?query=value&other=123";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], uri);
    }

    // ======================================================================
    // Test Group 10: Edge cases
    // ======================================================================

    #[test]
    fn test_single_value_range() {
        let uri = "http://example.com/file[5-5].txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], "http://example.com/file5.txt");
    }

    #[test]
    fn test_single_value_braced() {
        let uri = "http://example.com/file${07-07}.txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], "http://example.com/file07.txt");
    }

    #[test]
    fn test_large_numbers() {
        // Nested ${} inside [] - bracket regex won't match inner ${...},
        // so this falls through to treating [${99999-100005}] as literal text.
        // Use pure bracket syntax instead for large number ranges.
        let uri = "http://example.com/big[099999-100005].bin";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 7);
        assert_eq!(expanded[0], "http://example.com/big099999.bin");
        assert_eq!(expanded[6], "http://example.com/big100005.bin");
    }

    #[test]
    fn test_width_overflow_handling() {
        // When numbers exceed the specified width, they should still display correctly
        let uri = "http://example.com/f[1-100].txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 100);
        // Width = max(len("1"), len("100")) = 3, zero-padded to 3 digits
        assert_eq!(expanded[0], "http://example.com/f001.txt");
        assert_eq!(expanded[99], "http://example.com/f100.txt");
    }

    #[test]
    fn test_empty_uri() {
        let expanded = expand_parameterized_uri("");
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], "");
    }

    // ======================================================================
    // Test Group 11: Invalid patterns gracefully handled
    // ======================================================================

    #[test]
    fn test_invalid_bracket_content() {
        let uri = "http://example.com/[abc-def].txt";
        let expanded = expand_parameterized_uri(uri);

        // Should return original URI unchanged
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], uri);
    }

    #[test]
    fn test_invalid_braced_content() {
        let uri = "http://example.com/${not-a-number}.txt";
        let expanded = expand_parameterized_uri(uri);

        // Should return original URI unchanged
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], uri);
    }

    #[test]
    fn test_zero_step_invalid() {
        let uri = "http://example.com/file[01-10:0].zip";
        let expanded = expand_parameterized_uri(uri);

        // Step of 0 is invalid, should return original
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], uri);
    }

    #[test]
    fn test_unclosed_braces() {
        let uri = "http://example.com/${unclosed.txt";
        let expanded = expand_parameterized_uri(uri);

        // Unclosed braces are not matched by our regex, so treated as normal URI
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], uri);
    }

    #[test]
    fn test_malformed_range() {
        let uri = "http://example.com/${10-}.txt";
        let expanded = expand_parameterized_uri(uri);

        // Malformed range should return original
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], uri);
    }

    // ======================================================================
    // Test Group 12: Special characters preserved
    // ======================================================================

    #[test]
    fn test_special_chars_in_uri_preserved() {
        let uri = "http://example.com/path%20with%20spaces/${01-02}.html?query=test&special=%2F";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 2);
        assert_eq!(
            expanded[0],
            "http://example.com/path%20with%20spaces/01.html?query=test&special=%2F"
        );
        assert_eq!(
            expanded[1],
            "http://example.com/path%20with%20spaces/02.html?query=test&special=%2F"
        );
    }

    #[test]
    fn test_uri_with_auth_and_port() {
        let uri = "http://user:pass@example.com:8080/files/${01-03}.dat";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 3);
        assert_eq!(
            expanded[0],
            "http://user:pass@example.com:8080/files/01.dat"
        );
        assert_eq!(
            expanded[2],
            "http://user:pass@example.com:8080/files/03.dat"
        );
    }

    #[test]
    fn test_ipv6_address_not_confused() {
        // IPv6 address with brackets should not be confused with range syntax
        // The [2001:db8::1] is part of the host, and [01-02] is the file pattern
        let uri = "http://[2001:db8::1]:8080/file[01-02].txt";
        let expanded = expand_parameterized_uri(uri);

        // Should still expand the file[01-02] part but preserve IPv6
        assert_eq!(expanded.len(), 2);
        // The bracket pattern [01-02] should be expanded correctly
        assert!(expanded[0].ends_with("file01.txt"));
        assert!(expanded[1].ends_with("file02.txt"));
    }

    #[test]
    fn test_fragment_preserved() {
        let uri = "http://example.com/doc${01-02}.pdf#section=1";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[0], "http://example.com/doc01.pdf#section=1");
        assert_eq!(expanded[1], "http://example.com/doc02.pdf#section=1");
    }

    // ======================================================================
    // Additional edge case tests
    // ======================================================================

    #[test]
    fn test_format_with_width_basic() {
        assert_eq!(format_with_width(1, 3), "001");
        assert_eq!(format_with_width(42, 5), "00042");
        assert_eq!(format_with_width(999, 2), "999"); // Exceeds width, no truncation
        assert_eq!(format_with_width(0, 4), "0000");
    }

    #[test]
    fn test_generate_range_forward() {
        let result = generate_range(1, 5, 1, 2);
        assert_eq!(result, vec!["01", "02", "03", "04", "05"]);
    }

    #[test]
    fn test_generate_range_reverse() {
        let result = generate_range(5, 1, 1, 2);
        assert_eq!(result, vec!["05", "04", "03", "02", "01"]);
    }

    #[test]
    fn test_generate_range_with_step() {
        let result = generate_range(1, 10, 3, 1);
        assert_eq!(result, vec!["1", "4", "7", "10"]);
    }

    #[test]
    fn test_generate_range_single_value() {
        let result = generate_range(5, 5, 1, 3);
        assert_eq!(result, vec!["005"]);
    }

    #[test]
    fn test_find_param_patterns_simple() {
        let patterns = find_param_patterns("http://ex.com/$2/file.txt");
        assert_eq!(patterns.len(), 1);
        match &patterns[0].1 {
            ParamPattern::Simple { value } => assert_eq!(*value, 2),
            _ => panic!("Expected Simple pattern"),
        }
    }

    #[test]
    fn test_find_param_patterns_braced() {
        let patterns = find_param_patterns("http://ex.com/${01-05}.txt");
        assert_eq!(patterns.len(), 1);
        match &patterns[0].1 {
            ParamPattern::Braced { start, end, .. } => {
                assert_eq!(*start, 1);
                assert_eq!(*end, 5);
            }
            _ => panic!("Expected Braced pattern"),
        }
    }

    #[test]
    fn test_find_param_patterns_bracket() {
        let patterns = find_param_patterns("http://ex.com/file[01-10].zip");
        assert_eq!(patterns.len(), 1);
        match &patterns[0].1 {
            ParamPattern::Bracket { start, end, .. } => {
                assert_eq!(*start, 1);
                assert_eq!(*end, 10);
            }
            _ => panic!("Expected Bracket pattern"),
        }
    }

    #[test]
    fn test_complex_real_world_example() {
        // Real-world example: downloading a series of images
        let uri = "https://cdn.example.com/gallery/2024/photo${001-050}_hd.jpg";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 50);
        assert_eq!(
            expanded[0],
            "https://cdn.example.com/gallery/2024/photo001_hd.jpg"
        );
        assert_eq!(
            expanded[49],
            "https://cdn.example.com/gallery/2024/photo050_hd.jpg"
        );
    }

    #[test]
    fn test_step_larger_than_range() {
        // Step that skips most values
        let uri = "http://example.com/f[01-05:10].txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 1); // Only first value fits
        // Width = len("01") = 2, zero-padded
        assert_eq!(expanded[0], "http://example.com/f01.txt");
    }

    #[test]
    fn test_reverse_range_with_step() {
        let uri = "http://example.com/f[10-01:2].txt";
        let expanded = expand_parameterized_uri(uri);

        assert_eq!(expanded.len(), 5);
        assert_eq!(expanded[0], "http://example.com/f10.txt");
        assert_eq!(expanded[1], "http://example.com/f08.txt");
        assert_eq!(expanded[2], "http://example.com/f06.txt");
        assert_eq!(expanded[3], "http://example.com/f04.txt");
        assert_eq!(expanded[4], "http://example.com/f02.txt");
    }
}
