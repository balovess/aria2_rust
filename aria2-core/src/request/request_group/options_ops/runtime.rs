use std::collections::HashMap;
use std::sync::Arc;

use crate::rate_limiter::RateLimiter;
use crate::util::rwlock_ext::RwLockRecover;

use super::rpc_update::{RuntimeOptionChanges, apply_rpc_option};

impl super::super::RequestGroup {
    // ── Rate Limiter ────────────────────────────────────────────────────

    /// Store a handle to the download's `RateLimiter` so that runtime option
    /// updates (e.g. via `aria2.changeOption`) can dynamically adjust the rate.
    pub fn set_rate_limiter(&self, limiter: RateLimiter) {
        *self.rate_limiter.recover_mut() = Some(limiter);
    }

    /// Store options that take effect when the next command generation starts.
    pub fn set_pending_options(
        &self,
        changes: std::collections::HashMap<String, serde_json::Value>,
    ) {
        if let Ok(mut pending) = self.pending_options.write() {
            pending.extend(changes);
        }
    }

    /// Apply and clear options deferred by `changeOption`.
    pub fn apply_pending_options(&mut self) {
        let changes = self
            .pending_options
            .write()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for (key, value) in changes {
            self.update_option(&key, value);
        }
    }

    pub fn pending_options(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.pending_options
            .read()
            .map(|pending| pending.clone())
            .unwrap_or_default()
    }

    /// Return the task-level overrides that have actually been applied.
    pub fn runtime_options(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.runtime_options
            .read()
            .map(|options| options.clone())
            .unwrap_or_default()
    }

    /// Validate and partition a batch using the same policy for every
    /// external adapter. Waiting and paused groups are reserved; only a
    /// group in the `Active` state receives pending changes.
    pub(crate) fn classify_runtime_options(
        &self,
        changes: HashMap<String, serde_json::Value>,
    ) -> Result<RuntimeOptionChanges, String> {
        let is_running = self.status().is_running();
        let mut classified = RuntimeOptionChanges::default();
        for (key, value) in changes {
            match crate::config::is_option_changeable(&key, is_running) {
                crate::config::ChangeableKind::Immediate => {
                    if Self::validate_option_update(&key, &value)? {
                        classified.immediate.insert(key, value);
                    }
                }
                crate::config::ChangeableKind::Pending => {
                    if Self::validate_option_update(&key, &value)? {
                        classified.pending.insert(key, value);
                    }
                }
                crate::config::ChangeableKind::NotChangeable => {}
            }
        }
        Ok(classified)
    }

    /// Apply a previously classified immediate batch. Validation is repeated
    /// at this seam so direct core callers cannot bypass the runtime contract.
    pub(crate) fn apply_runtime_options(
        &mut self,
        changes: HashMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        for (key, value) in changes {
            if !self.try_update_option(&key, value)? {
                return Err(format!("Option '{}' cannot be changed at runtime", key));
            }
        }
        Ok(())
    }

    // ── Runtime Option Updates ──────────────────────────────────────────

    /// Update a single runtime-changeable option by key (using aria2's
    /// kebab-case option names, e.g. `"max-download-limit"`).
    ///
    /// Returns `true` if the option was recognized and updated, `false` if the
    /// key is not a runtime-changeable option. Invalid values are reported by
    /// [`Self::try_update_option`] and are intentionally not hidden here.
    ///
    /// For `max-download-limit` / `max-upload-limit`, the stored
    /// `RateLimiter` (if any) is also updated so the change takes effect
    /// immediately on the live download.
    pub fn validate_option_update(key: &str, value: &serde_json::Value) -> Result<bool, String> {
        let registry = crate::config::OptionRegistry::new();
        if registry.get(key).is_some() {
            registry
                .parse_rpc_value(key, value)
                .map_err(|error| format!("Option '{}': {}", key, error))?;
        }
        let mut options = super::super::DownloadOptions::default();
        apply_rpc_option(&mut options, key, value)
    }

    /// Apply a runtime option while preserving parse failures for RPC callers.
    pub fn try_update_option(
        &mut self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<bool, String> {
        if !Self::validate_option_update(key, &value)? {
            return Ok(false);
        }

        let opts = Arc::make_mut(&mut self.options);
        let applied = apply_rpc_option(opts, key, &value)?;

        match key {
            "max-download-limit" => {
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_download_rate(opts.max_download_limit);
                }
            }
            "max-upload-limit" => {
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_upload_rate(opts.max_upload_limit);
                }
            }
            "split" => {
                tracing::warn!(
                    new_split = opts.split,
                    "split changed but will take effect on download restart/retry, \
                     not mid-download (current segments unchanged)"
                );
            }
            _ => {}
        }
        if applied {
            if let Ok(mut runtime_options) = self.runtime_options.write() {
                runtime_options.insert(key.to_string(), value);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Compatibility wrapper for internal callers that only need to know
    /// whether a key is recognized. RPC-facing code should use
    /// [`Self::try_update_option`] so invalid values cannot be swallowed.
    pub fn update_option(&mut self, key: &str, value: serde_json::Value) -> bool {
        self.try_update_option(key, value).unwrap_or(false)
    }
}
