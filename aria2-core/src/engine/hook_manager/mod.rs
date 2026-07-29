//! BitTorrent post-download hook system
//!
//! Provides custom processing capabilities after download completion, supporting file moving,
//! renaming, timestamp updating, and external command execution.
//! HookManager manages the execution chain of multiple hooks and supports configuring error handling strategies.

pub mod builtin;
pub mod types;

#[cfg(test)]
mod tests;

// Re-export all public types from sub-modules for backward compatibility
pub use types::{DownloadStats, DownloadStatus, HookContext, PostDownloadHook};
pub use builtin::{ExecHook, MoveHook, RenameHook, TouchHook};

use tracing::{debug, error, info};

use crate::error::{Aria2Error, Result};

// ============================================================================
// HookConfig
// ============================================================================

/// Hook system configuration
#[derive(Clone, Debug)]
pub struct HookConfig {
    /// Whether to stop subsequent hook execution on error
    pub stop_on_error: bool,
    /// Timeout for individual hook execution
    pub timeout: std::time::Duration,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            stop_on_error: false,
            timeout: std::time::Duration::from_secs(30),
        }
    }
}

// ============================================================================
// HookManager - Hook chain manager
// ============================================================================

/// Hook chain manager
///
/// Responsible for managing and coordinating the execution of multiple post-download hooks.
/// Supports sequential execution of the hook chain, and determines the error handling strategy
/// based on configuration.
pub struct HookManager {
    /// Registered hook list (executed in registration order)
    hooks: Vec<Box<dyn PostDownloadHook>>,
    /// Hook system configuration
    config: HookConfig,
}

impl HookManager {
    /// Create a new hook manager
    ///
    /// # Arguments
    ///
    /// * `config` - Hook system configuration options
    pub fn new(config: HookConfig) -> Self {
        Self {
            hooks: Vec::new(),
            config,
        }
    }

    /// Add a new hook to the hook chain
    ///
    /// Hooks are executed in the order they are added.
    ///
    /// # Arguments
    ///
    /// * `hook` - Hook instance to add (must implement the `PostDownloadHook` trait)
    pub fn add_hook(&mut self, hook: Box<dyn PostDownloadHook>) {
        info!(hook_name = hook.name(), "Adding hook to chain");
        self.hooks.push(hook);
    }

    /// Remove hook by name
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the hook to remove
    ///
    /// # Returns
    ///
    /// Returns the removed hook (if found), otherwise returns `None`
    pub fn remove_hook(&mut self, name: &str) -> Option<Box<dyn PostDownloadHook>> {
        let pos = self.hooks.iter().position(|h| h.name() == name)?;
        info!(hook_name = name, "Removing hook from chain");
        Some(self.hooks.remove(pos))
    }

    /// Trigger on_complete callback for all hooks
    ///
    /// Calls each hook's `on_complete` method in registration order.
    /// Based on `config.stop_on_error`, decides whether to stop on the first failure.
    ///
    /// # Arguments
    ///
    /// * `context` - Download completion context information
    ///
    /// # Returns
    ///
    /// Returns a vector of execution result descriptions for each hook.
    /// If `stop_on_error=true` and a hook fails, returns `Err` containing the error message.
    pub async fn fire_complete(&self, context: &HookContext) -> Result<Vec<String>> {
        let mut results = Vec::with_capacity(self.hooks.len());

        for hook in &self.hooks {
            let hook_name = hook.name();
            debug!(hook = hook_name, event = "complete", "Executing hook");

            match hook.on_complete(context).await {
                Ok(()) => {
                    let msg = format!("[{}] complete succeeded", hook_name);
                    info!("{}", msg);
                    results.push(msg);
                }
                Err(e) => {
                    let msg = format!("[{}] complete failed: {}", hook_name, e);
                    error!("{}", msg);

                    if self.config.stop_on_error {
                        return Err(Aria2Error::DownloadFailed(format!(
                            "Hook '{}' execution aborted due to stop_on_error setting: {}",
                            hook_name, e
                        )));
                    }

                    results.push(msg);
                }
            }
        }

        Ok(results)
    }

    /// Trigger on_error callback for all hooks
    ///
    /// Similar to `fire_complete`, but calls the `on_error` method instead.
    ///
    /// # Arguments
    ///
    /// * `context` - Download failure context information
    /// * `error` - Error description string
    ///
    /// # Returns
    ///
    /// Returns a vector of execution result descriptions for each hook
    pub async fn fire_error(&self, context: &HookContext, error: &str) -> Result<Vec<String>> {
        let mut results = Vec::with_capacity(self.hooks.len());
        let error_owned = error.to_string();

        for hook in &self.hooks {
            let hook_name = hook.name();
            debug!(hook = hook_name, event = "error", "Executing hook");

            match hook.on_error(context, &error_owned).await {
                Ok(()) => {
                    let msg = format!("[{}] error handled successfully", hook_name);
                    info!("{}", msg);
                    results.push(msg);
                }
                Err(e) => {
                    let msg = format!("[{}] error handling failed: {}", hook_name, e);
                    error!("{}", msg);

                    if self.config.stop_on_error {
                        return Err(Aria2Error::DownloadFailed(format!(
                            "Hook '{}' error handler aborted due to stop_on_error setting: {}",
                            hook_name, e
                        )));
                    }

                    results.push(msg);
                }
            }
        }

        Ok(results)
    }

    /// Get the number of currently registered hooks
    pub fn hook_count(&self) -> usize {
        self.hooks.len()
    }

    /// Clear all registered hooks
    pub fn clear_hooks(&mut self) {
        info!("Clearing all hooks");
        self.hooks.clear();
    }
}
