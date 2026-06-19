//! Configuration loading and management for the App
//!
//! This module handles all configuration-related operations:
//! - Command-line argument parsing
//! - Environment variable loading
//! - Configuration file loading
//! - Option retrieval helpers

use super::App;
use aria2_core::config::{OptionValue, UriListFile};
use aria2_core::validation::protocol_detector::detect;
use tracing::warn;

impl App {
    /// Load command-line arguments into the configuration.
    ///
    /// Parses both short (-d) and long (--dir) options, handles:
    /// - Boolean flags (--daemon)
    /// - String values (--dir=/downloads or --dir /downloads)
    /// - Negation (--no-check-certificate)
    /// - URI list files (@file.txt)
    /// - Positional URIs
    pub async fn load_args(&mut self, args: &[String]) -> std::result::Result<(), String> {
        let mut conf = self.config.write().await;

        let mut positional_uris = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg.starts_with('-') && !arg.starts_with("--") && arg.len() == 2 {
                let c = arg.chars().nth(1).unwrap_or('\0');
                if let Some(opt_name) = self.map_short_option(c) {
                    if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                        conf.set_global_option(opt_name, OptionValue::Str(args[i + 1].clone()))
                            .await
                            .map_err(|e| format!("Option {} error: {}", opt_name, e))?;
                        i += 2;
                        continue;
                    } else {
                        conf.set_global_option(opt_name, OptionValue::Bool(true))
                            .await
                            .map_err(|e| format!("Option {} error: {}", opt_name, e))?;
                        i += 1;
                        continue;
                    }
                }
            } else if let Some(opt_str) = arg.strip_prefix("--") {
                if opt_str == "help" || opt_str == "h" || opt_str == "version" || opt_str == "V" {
                    i += 1;
                    continue;
                }
                let (opt_name, value) = if let Some(eq_pos) = opt_str.find('=') {
                    (&opt_str[..eq_pos], Some(&opt_str[eq_pos + 1..]))
                } else {
                    (opt_str, None)
                };

                let actual_name = if opt_name.starts_with("no-") && opt_name.len() > 3 {
                    &opt_name[3..]
                } else {
                    opt_name
                };

                if let Some(val) = value {
                    if opt_name.starts_with("no-") {
                        conf.set_global_option(actual_name, OptionValue::Bool(false))
                            .await
                            .map_err(|e| format!("Option {} error: {}", opt_name, e))?;
                    } else {
                        conf.set_global_option(actual_name, OptionValue::Str(val.to_string()))
                            .await
                            .map_err(|e| format!("Option {} error: {}", opt_name, e))?;
                    }
                } else if opt_name.starts_with("no-") {
                    conf.set_global_option(actual_name, OptionValue::Bool(false))
                        .await
                        .map_err(|e| format!("Option {} error: {}", opt_name, e))?;
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    conf.set_global_option(opt_name, OptionValue::Str(args[i + 1].clone()))
                        .await
                        .map_err(|e| format!("Option {} error: {}", opt_name, e))?;
                    i += 1;
                    i += 1;
                    continue;
                } else {
                    conf.set_global_option(opt_name, OptionValue::Bool(true))
                        .await
                        .map_err(|e| format!("Option {} error: {}", opt_name, e))?;
                }

                i += 1;
                continue;
            } else if let Some(path) = arg.strip_prefix('@') {
                match UriListFile::from_file(path) {
                    Ok(uri_list) => {
                        for entry in uri_list.entries() {
                            for uri in &entry.uris {
                                positional_uris.push(uri.clone());
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to load URI list file {}: {}", path, e);
                    }
                }
                i += 1;
                continue;
            } else {
                positional_uris.push(arg.clone());
            }
            i += 1;
        }

        drop(conf);

        let input_file = self.get_opt_str("input-file").await;
        if let Some(path) = input_file {
            match UriListFile::from_file(&path) {
                Ok(uri_list) => {
                    for entry in uri_list.entries() {
                        for uri in &entry.uris {
                            positional_uris.push(uri.clone());
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to load input-file {}: {}", path, e);
                }
            }
        }

        self.detected_inputs = positional_uris
            .into_iter()
            .filter_map(|uri| match detect(&uri) {
                Ok(d) => Some(d),
                Err(e) => {
                    warn!("Cannot detect input type '{}': {}", uri, e);
                    None
                }
            })
            .collect();
        Ok(())
    }

    /// Load configuration from environment variables.
    pub async fn load_env(&mut self) {
        let mut conf = self.config.write().await;
        conf.load_env().await;
    }

    /// Load configuration from a file.
    ///
    /// If no path is provided, looks for ~/.aria2/aria2.conf
    pub async fn load_config_file(
        &mut self,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        let conf_path = if let Some(p) = path {
            p.to_string()
        } else {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| ".".to_string());

            let candidate = format!("{}/{}/{}", home, crate::constants::CONFIG_DIR_NAME, crate::constants::CONFIG_FILE_NAME);
            if std::path::Path::new(&candidate).exists() {
                candidate
            } else {
                return Ok(());
            }
        };

        let mut conf = self.config.write().await;
        conf.load_file(&conf_path).await;
        Ok(())
    }

    /// Get a string option value.
    pub(super) async fn get_opt_str(&self, name: &str) -> Option<String> {
        self.config.read().await.get_global_str(name).await
    }

    /// Get an integer option value.
    pub(super) async fn get_opt_i64(&self, name: &str) -> Option<i64> {
        self.config.read().await.get_global_i64(name).await
    }

    /// Get an usize option value.
    pub(super) async fn get_opt_usize(&self, name: &str) -> Option<usize> {
        self.config
            .read()
            .await
            .get_global_i64(name)
            .await
            .map(|v| v as usize)
    }

    /// Get a boolean option value.
    pub(super) async fn get_opt_bool(&self, name: &str) -> Option<bool> {
        self.config.read().await.get_global_bool(name).await
    }
}
