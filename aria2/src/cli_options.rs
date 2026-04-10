//! CLI Options Parser - Command-line argument handling
//!
//! This module provides utilities for parsing and validating command-line
//! arguments for the aria2-rust CLI application.
//!
//! # Features
//!
//! - Short option mapping (`-d` → `--dir`)
//! - Long option parsing with value detection
//! - Positional URI extraction
//! - Option validation and normalization
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/OptionParser.cc/h` - Option parsing logic
//! - `src/option.h` - Option definitions

use std::collections::HashMap;
use tracing::{debug, warn};

/// Short option to long option name mapping
pub fn get_short_option_map() -> HashMap<char, &'static str> {
    let mut map = HashMap::new();
    // Directory options
    map.insert('d', "dir");
    map.insert('o', "out");
    map.insert('l', "log");
    map.insert('D', "daemon");

    // Connection options
    map.insert('x', "max-connection-per-server");
    map.insert('s', "split");
    map.insert('k', "min-split-size");

    // BT options
    map.insert('t', "seed-time");
    map.insert('w', "bt-max-peers");

    // General options
    map.insert('q', "quiet");
    map.insert('v', "verbose");
    map.insert('V', "version");
    map.insert('h', "help");

    map
}

/// Parse command-line arguments into options and positional URIs
///
/// # Arguments
/// * `args` - Command-line arguments (excluding program name)
///
/// # Returns
/// * `Ok((options, uris))` - Tuple of option key-value pairs and URI list
/// * `Err(String)` - If argument parsing fails
///
/// # Example
///
/// ```rust,ignore
/// let (options, uris) = parse_args(&[
///     "--dir=/downloads".into(),
///     "-v".into(),
///     "http://example.com/file.zip".into(),
/// ])?;
/// assert_eq!(uris.len(), 1);
/// ```
pub fn parse_args(
    args: &[String],
) -> Result<(Vec<(String, Option<String>)>, Vec<String>), String> {
    let short_map = get_short_option_map();
    let mut options = Vec::new();
    let mut positional_uris = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() == 2 {
            // Short option (-d, -o, etc.)
            let c = arg.chars().nth(1).unwrap_or('\0');
            if let Some(opt_name) = short_map.get(&c) {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    options.push((opt_name.to_string(), Some(args[i + 1].clone())));
                    i += 2;
                    continue;
                } else {
                    options.push((opt_name.to_string(), None));
                    i += 1;
                    continue;
                }
            }
            warn!("Unknown short option: {}", arg);
            i += 1;
        } else if arg.starts_with("--") {
            // Long option (--dir=value, --verbose, etc.)
            let opt_str = &arg[2..];

            // Skip help/version flags
            if opt_str == "help" || opt_str == "h" || opt_str == "version" || opt_str == "V" {
                i += 1;
                continue;
            }

            let (opt_name, value) = if let Some(eq_pos) = opt_str.find('=') {
                (&opt_str[..eq_pos], Some(&opt_str[eq_pos + 1..]))
            } else {
                (opt_str, None)
            };

            // Handle boolean flags without values
            if value.is_none()
                && i + 1 < args.len()
                && (args[i + 1] == "true" || args[i + 1] == "false")
            {
                options.push((opt_name.to_string(), Some(args[i + 1].clone())));
                i += 2;
                continue;
            }

            options.push((opt_name.to_string().to_string(), value.map(|s| s.to_string())));
            i += 1;
        } else {
            // Positional argument (URI or file path)
            positional_uris.push(arg.clone());
            i += 1;
        }
    }

    debug!(
        "Parsed {} options, {} positional URIs",
        options.len(),
        positional_uris.len()
    );

    Ok((options, positional_uris))
}

/// Validate parsed options for common errors
///
/// # Arguments
/// * `options` - Slice of (name, value) pairs
///
/// # Returns
/// * `Ok(())` - Validation passed
/// * `Err(String)` - Description of validation error
pub fn validate_options(options: &[(String, Option<String>)]) -> Result<(), String> {
    for (name, value) in options {
        match name.as_str() {
            "dir" => {
                if let Some(dir) = value {
                    if dir.is_empty() {
                        return Err("Directory path cannot be empty".into());
                    }
                }
            }
            "max-connection-per-server" | "split" => {
                if let Some(val) = value {
                    if let Ok(n) = val.parse::<usize>() {
                        if n == 0 || n > 16 {
                            return Err(format!("{} must be between 1 and 16", name));
                        }
                    } else {
                        return Err(format!("{} must be a positive integer", name));
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Extract URIs from arguments that look like URLs or file paths
///
/// Filters out non-URI positional arguments.
///
/// # Arguments
/// * `positional` - List of positional arguments from CLI
///
/// # Returns
/// * Vector of valid URI strings
pub fn extract_uris(positional: &[String]) -> Vec<String> {
    positional
        .iter()
        .filter(|arg| {
            arg.starts_with("http://")
                || arg.starts_with("https://")
                || arg.starts_with("ftp://")
                || arg.starts_with("ftps://")
                || arg.ends_with(".torrent")
                || arg.ends_with(".metalink")
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_option_map_contains_common_options() {
        let map = get_short_option_map();
        assert!(map.contains_key(&'d')); // dir
        assert!(map.contains_key(&'o')); // out
        assert!(map.contains_key(&'v')); // verbose
    }

    #[test]
    fn test_parse_args_basic() {
        let args = vec![
            "--dir=/downloads".into(),
            "http://example.com/file.zip".into(),
        ];
        let (options, uris) = parse_args(&args).unwrap();
        assert_eq!(options.len(), 1);
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0], "http://example.com/file.zip");
    }

    #[test]
    fn test_parse_args_short_options() {
        let args = vec!["-d".into(), "/tmp".into(), "-v".into()];
        let (options, _) = parse_args(&args).unwrap();
        assert_eq!(options.len(), 2); // -d /tmp, -v
    }

    #[test]
    fn test_parse_args_mixed() {
        let args = vec![
            "--dir=/downloads".into(),
            "-v".into(),
            "--max-connection-per-server=4".into(),
            "http://example.com/a.zip".into(),
            "http://example.com/b.torrent".into(),
        ];
        let (options, uris) = parse_args(&args).unwrap();
        assert_eq!(options.len(), 3);
        assert_eq!(uris.len(), 2);
    }

    #[test]
    fn test_validate_options_valid() {
        let options = vec![
            ("dir".into(), Some("/downloads".into())),
            ("max-connection-per-server".into(), Some("4".into())),
        ];
        assert!(validate_options(&options).is_ok());
    }

    #[test]
    fn test_validate_options_invalid_dir() {
        let options = vec![("dir".into(), Some("".into()))];
        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn test_validate_options_invalid_split() {
        let options = vec![("split".into(), Some("100".into()))]; // > 16
        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn test_extract_uris_http() {
        let positional = vec![
            "http://example.com/file.zip".into(),
            "https://example.com/file2.zip".into(),
            "not-a-uri.txt".into(),
        ];
        let uris = extract_uris(&positional);
        assert_eq!(uris.len(), 2);
    }

    #[test]
    fn test_extract_uris_torrent() {
        let positional = vec![
            "/path/to/file.torrent".into(),
            "/path/to/other.metalink".into(),
            "/path/to/text.txt".into(),
        ];
        let uris = extract_uris(&positional);
        assert_eq!(uris.len(), 2);
    }
}
