//! Configuration loading and management for the App
//!
//! This module wires the App configuration operations together. The CLI
//! mapping, source loading, and typed option access live in focused submodules.

mod cli;
mod loader;
mod options;

use super::App;
use super::config_maintenance;
use std::path::Path;

impl App {
    /// Validate startup configuration without initializing the download
    /// engine or requiring a positional URI.
    pub async fn check_config(
        &mut self,
        no_conf: bool,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        self.load_startup_config(no_conf, path).await
    }

    /// Disable only invalid configuration entries while retaining a backup.
    pub fn repair_config_file(path: &Path) -> Result<(std::path::PathBuf, usize), String> {
        let update = config_maintenance::repair(path)?;
        Ok((update.backup_path, update.changed_lines))
    }

    /// Replace a configuration file with the built-in defaults after backing it up.
    pub fn reset_config_file(path: &Path) -> Result<std::path::PathBuf, String> {
        let update = config_maintenance::reset(path)?;
        Ok(update.backup_path)
    }
}
