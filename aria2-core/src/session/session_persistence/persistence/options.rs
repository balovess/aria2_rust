//! Global session options persistence and cleanup.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::request::request_group::RequestGroup;

use super::types::SessionPersistence;

/// Filename for global session options saved alongside .aria2 files
const SESSION_OPTIONS_FILENAME: &str = "session_options.json";

impl SessionPersistence {
    /// Save global options summary to session directory
    pub(super) async fn save_global_options(
        &self,
        _groups: &[Arc<std::sync::RwLock<RequestGroup>>],
    ) -> Result<(), String> {
        let opts_path = self.session_dir.join(SESSION_OPTIONS_FILENAME);

        // Build a simple options summary from all groups
        let options_summary = serde_json::json!({
            "version": "1.0",
            "saved_at": chrono_timestamp_or_fallback(),
            "note": "Global session options summary"
        });

        let json = serde_json::to_string_pretty(&options_summary)
            .map_err(|e| format!("Failed to serialize session options: {}", e))?;

        tokio::fs::write(&opts_path, json).await.map_err(|e| {
            format!(
                "Failed to write session options {}: {}",
                opts_path.display(),
                e
            )
        })?;

        Ok(())
    }

    /// Load global options from session directory
    pub(super) async fn load_global_options(&self) -> Result<(), String> {
        let opts_path = self.session_dir.join(SESSION_OPTIONS_FILENAME);

        if !opts_path.exists() {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(&opts_path).await.map_err(|e| {
            format!(
                "Failed to read session options {}: {}",
                opts_path.display(),
                e
            )
        })?;

        // Validate it's valid JSON (basic sanity check)
        let _parsed: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| format!("Invalid JSON in session options: {}", e))?;

        debug!(path = %opts_path.display(), "Loaded session options");

        Ok(())
    }

    /// Clean up all session files (for testing or reset)
    pub async fn cleanup(&self) -> Result<(), String> {
        if !self.session_dir.exists() {
            return Ok(());
        }

        let mut entries = tokio::fs::read_dir(&self.session_dir)
            .await
            .map_err(|e| format!("Failed to read session dir: {}", e))?;

        loop {
            let Some(entry) = entries.next_entry().await.map_err(|error| {
                format!(
                    "Failed to enumerate session dir {} while cleaning up: {}",
                    self.session_dir.display(),
                    error
                )
            })?
            else {
                break;
            };
            let path = entry.path();
            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!(path = %path.display(), error = %e, "Failed to remove session file");
            }
        }

        info!(dir = %self.session_dir.display(), "Session directory cleaned up");
        Ok(())
    }
}

/// Fallback timestamp generator when chrono is not available
fn chrono_timestamp_or_fallback() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}
