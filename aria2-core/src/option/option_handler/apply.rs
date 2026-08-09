//! Applying options to download contexts.
//!
//! Implements [`OptionHandlerApply`] — a trait that extends [`OptionHandler`]
//! with methods for loading config files, applying CLI argument overrides,
//! and converting options into [`DownloadOptions`] for download commands.

use std::path::Path;

use crate::config::option::OptionValue;
use crate::request::request_group::DownloadOptions;

use super::OptionHandler;
use super::parsing::{detect_value_type, parse_kv_arg};
use super::validation::parse_config_line;

/// Extension trait for [`OptionHandler`] that provides option application methods.
///
/// Separated into a dedicated module to keep the core struct definition
/// and simple accessors in [`mod.rs`] clean.
pub trait OptionHandlerApply {
    /// Override options from raw command-line arguments.
    ///
    /// Parses common CLI patterns:
    /// - `--key=value` / `--key:value`
    /// - `--key value` (value in next arg)
    /// - `--no-key` sets boolean to false
    /// - `-o key=value` (GNU style)
    ///
    /// CLI arguments take precedence over config file values but can be
    /// overridden by explicit [`set`](OptionHandler::set) calls.
    fn apply_args(&mut self, args: &[String]);

    /// Load options from a `.aria2rc` config file.
    ///
    /// File format:
    /// ```text
    /// # Comment lines start with #
    /// key=value
    /// key="value with spaces"
    /// key=['val1', 'val2']
    /// bool-key=true
    /// number-key=42
    /// float-key=3.14
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error only if the file cannot be read (IO failure).
    fn load_config_file(&mut self, path: &Path) -> Result<(), String>;

    /// Convert current options to a [`DownloadOptions`] struct suitable for
    /// creating a download task.
    fn to_download_options(&self) -> DownloadOptions;
}

impl OptionHandlerApply for OptionHandler {
    fn apply_args(&mut self, args: &[String]) {
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];

            // Skip non-option arguments (e.g., URLs, positional args)
            if !arg.starts_with('-') || arg == "--" {
                i += 1;
                continue;
            }

            // Parse --key=value or --key:value
            if let Some((key, value)) = parse_kv_arg(arg) {
                if let Some(parsed) = detect_value_type(value.trim()) {
                    tracing::debug!(key, value = ?parsed, "CLI arg applied");
                    self.set(key, parsed);
                }
                i += 1;
                continue;
            }

            // Parse --no-key (boolean false)
            if let Some(key) = arg.strip_prefix("--no-") {
                self.set(key, OptionValue::Bool(false));
                i += 1;
                continue;
            }

            // Parse --key <next-arg> (value in next argument)
            if let Some(key) = arg.strip_prefix("--") {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    let value = &args[i + 1];
                    if let Some(parsed) = detect_value_type(value) {
                        self.set(key, parsed);
                    }
                    i += 2;
                    continue;
                } else {
                    // Flag without value: treat as boolean true
                    self.set(key, OptionValue::Bool(true));
                    i += 1;
                    continue;
                }
            }

            // Parse -o key=value
            if arg == "-o" && i + 1 < args.len() {
                let next = &args[i + 1];
                if let Some((key, value)) = next.split_once('=')
                    && let Some(parsed) = detect_value_type(value.trim())
                {
                    self.set(key, parsed);
                }
                i += 2;
                continue;
            }

            i += 1;
        }
    }

    fn load_config_file(&mut self, path: &Path) -> Result<(), String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file '{}': {}", path.display(), e))?;

        let path_display = path.display().to_string();

        for (line_num, raw_line) in content.lines().enumerate() {
            if let Some((key, value_str)) = parse_config_line(raw_line, &path_display, line_num + 1)
            {
                // Auto-detect type and set
                match detect_value_type(value_str) {
                    Some(parsed) => {
                        tracing::debug!(
                            key,
                            value = ?parsed,
                            source = %path_display,
                            "Config option loaded"
                        );
                        self.set(key, parsed);
                    }
                    None => {
                        tracing::warn!(
                            key,
                            line = line_num + 1,
                            source = %path_display,
                            "Failed to parse config value"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn to_download_options(&self) -> DownloadOptions {
        DownloadOptions::from_option_values(&self.to_map())
    }
}
