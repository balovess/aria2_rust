//! Built-in hook implementations
//!
//! Provides standard post-download hooks:
//! - `MoveHook`: Move downloaded files to a target directory
//! - `RenameHook`: Rename files using a template pattern
//! - `TouchHook`: Update file timestamps
//! - `ExecHook`: Execute external commands with download metadata as env vars

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, Result};

use super::types::{HookContext, PostDownloadHook};

// ============================================================================
// MoveHook
// ============================================================================

/// File move hook
///
/// Moves the file to a specified directory after download completion.
/// Supports automatic creation of target directory structure.
#[derive(Clone, Debug)]
pub struct MoveHook {
    /// Target directory path
    target_dir: PathBuf,
    /// Whether to automatically create non-existent directories
    create_dirs: bool,
}

impl MoveHook {
    /// Create a new move hook
    ///
    /// # Arguments
    ///
    /// * `target_dir` - Target directory path
    /// * `create_dirs` - Whether to automatically create directories
    pub fn new(target_dir: PathBuf, create_dirs: bool) -> Self {
        Self {
            target_dir,
            create_dirs,
        }
    }
}

#[async_trait]
impl PostDownloadHook for MoveHook {
    /// Execute file move operation
    ///
    /// Verifies the source file exists, then moves it to the target directory.
    /// If `create_dirs` is true, the required directory hierarchy is created automatically.
    async fn on_complete(&self, context: &HookContext) -> Result<()> {
        let source = &context.file_path;

        // Verify source file exists
        if !source.exists() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
                path: source.to_string_lossy().to_string(),
            }));
        }

        // Create target directory (if needed)
        if self.create_dirs && !self.target_dir.exists() {
            debug!(
                hook = "MoveHook",
                target_dir = %self.target_dir.display(),
                "Creating target directory"
            );
            tokio::fs::create_dir_all(&self.target_dir)
                .await
                .map_err(|e| {
                    Aria2Error::Io(format!(
                        "Failed to create directory {}: {}",
                        self.target_dir.display(),
                        e
                    ))
                })?;
        }

        // Build destination path
        let filename = context.filename();
        let destination = self.target_dir.join(filename);

        info!(
            hook = "MoveHook",
            source = %source.display(),
            dest = %destination.display(),
            "Moving file"
        );

        // Execute move operation
        tokio::fs::rename(source, &destination).await.map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to move file from {} to {}: {}",
                source.display(),
                destination.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Move hook does nothing on error
    async fn on_error(&self, _context: &HookContext, _error: &str) -> Result<()> {
        // File move typically does not need to be executed on error
        Ok(())
    }

    fn name(&self) -> &'static str {
        "MoveHook"
    }
}

// ============================================================================
// RenameHook
// ============================================================================

/// File rename hook
///
/// Renames downloaded files using a template pattern.
/// Supported placeholders:
/// - `%d`: Source file directory
/// - `%f`: Original filename
/// - `%e`: File extension
/// - `%i`: Download task GID
/// - `%t`: Current timestamp (Unix timestamp)
#[derive(Clone, Debug)]
pub struct RenameHook {
    /// Rename template pattern
    pattern: String,
}

impl RenameHook {
    /// Create a new rename hook
    ///
    /// # Arguments
    ///
    /// * `pattern` - Rename template supporting placeholder substitution
    pub fn new(pattern: String) -> Self {
        Self { pattern }
    }

    /// Expand placeholders in the template
    ///
    /// Replaces special markers in the template string with actual values.
    ///
    /// # Arguments
    ///
    /// * `context` - Hook context used to obtain replacement values
    ///
    /// # Returns
    ///
    /// The expanded filename with all placeholders replaced
    pub fn expand_pattern(&self, context: &HookContext) -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.pattern
            .replace("%d", &context.directory().to_string_lossy())
            .replace("%f", context.filename())
            .replace("%e", context.extension())
            .replace("%i", &context.gid.value().to_string())
            .replace("%t", &timestamp.to_string())
    }
}

#[async_trait]
impl PostDownloadHook for RenameHook {
    /// Execute file rename operation
    ///
    /// Generates a new filename based on the template pattern and renames the file.
    async fn on_complete(&self, context: &HookContext) -> Result<()> {
        let source = &context.file_path;

        if !source.exists() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
                path: source.to_string_lossy().to_string(),
            }));
        }

        let new_name = self.expand_pattern(context);

        // If the new name contains a path separator, treat it as a full path; otherwise rename in the same directory
        let destination = if new_name.contains(std::path::MAIN_SEPARATOR)
            || (std::path::MAIN_SEPARATOR == '\\' && new_name.contains('/'))
        {
            PathBuf::from(&new_name)
        } else {
            context.directory().join(&new_name)
        };

        info!(
            hook = "RenameHook",
            source = %source.display(),
            dest = %destination.display(),
            pattern = %self.pattern,
            "Renaming file"
        );

        tokio::fs::rename(source, &destination).await.map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to rename file to {}: {}",
                destination.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Rename hook does nothing on error
    async fn on_error(&self, _context: &HookContext, _error: &str) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RenameHook"
    }
}

// ============================================================================
// TouchHook
// ============================================================================

/// File timestamp update hook
///
/// Updates the file's modification time and access time to the current system time after download completion.
#[derive(Clone, Debug)]
pub struct TouchHook;

impl TouchHook {
    /// Create a new TouchHook instance
    pub fn new() -> Self {
        Self
    }
}

impl Default for TouchHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PostDownloadHook for TouchHook {
    /// Update the file's modification time and access time
    ///
    /// Sets both the file's mtime and atime to the current system time.
    async fn on_complete(&self, context: &HookContext) -> Result<()> {
        let path = &context.file_path;

        if !path.exists() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
                path: path.to_string_lossy().to_string(),
            }));
        }

        let now = SystemTime::now();

        debug!(
            hook = "TouchHook",
            path = %path.display(),
            "Updating file timestamps"
        );

        // Use filetime crate or standard library to set timestamps
        #[cfg(unix)]
        {
            // Get existing permissions to preserve them
            let _metadata = tokio::fs::metadata(path).await.map_err(|e| {
                Aria2Error::Io(format!(
                    "Failed to get metadata for {}: {}",
                    path.display(),
                    e
                ))
            })?;

            // Use utimensat to set timestamps with nanosecond precision
            let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
            let times: [libc::timespec; 2] = [
                libc::timespec {
                    tv_sec: duration.as_secs() as _,
                    tv_nsec: duration.subsec_nanos() as _,
                },
                libc::timespec {
                    tv_sec: duration.as_secs() as _,
                    tv_nsec: duration.subsec_nanos() as _,
                },
            ];

            let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes())
                .map_err(|e| Aria2Error::Io(format!("Invalid path: {}", e)))?;

            unsafe {
                if libc::utimensat(0, c_path.as_ptr(), times.as_ptr(), 0) != 0 {
                    return Err(Aria2Error::Io(format!(
                        "Failed to update timestamps for {}",
                        path.display()
                    )));
                }
            }
        }

        #[cfg(windows)]
        {
            // On Windows, use the standard library's set_times feature (requires Rust 1.75+)
            // or update timestamps by re-writing the file
            use std::fs;

            // Simple approach: read file metadata and set time
            let file = fs::OpenOptions::new().write(true).open(path).map_err(|e| {
                Aria2Error::Io(format!("Failed to open file {}: {}", path.display(), e))
            })?;

            file.set_modified(now).map_err(|e| {
                Aria2Error::Io(format!(
                    "Failed to set modified time for {}: {}",
                    path.display(),
                    e
                ))
            })?;
        }

        info!(
            hook = "TouchHook",
            path = %path.display(),
            "File timestamps updated"
        );

        Ok(())
    }

    /// TouchHook does nothing on error
    async fn on_error(&self, _context: &HookContext, _error: &str) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "TouchHook"
    }
}

// ============================================================================
// ExecHook
// ============================================================================

/// External command execution hook
///
/// Executes a specified external command after download completion, injecting download-related
/// information as environment variables.
/// Supported environment variables:
/// - `ARIA2_GID`: Task group ID
/// - `ARIA2_PATH`: File path
/// - `ARIA2_STATUS`: Download status
/// - `ARIA2_ERROR`: Error message (if any)
/// - `ARIA2_DOWNLOADED_BYTES`: Downloaded bytes
/// - `ARIA2_UPLOADED_BYTES`: Uploaded bytes
/// - `ARIA2_DOWNLOAD_SPEED`: Download speed
/// - `ARIA2_UPLOAD_SPEED`: Upload speed
#[derive(Clone, Debug)]
pub struct ExecHook {
    /// Command to execute
    command: String,
    /// Additional environment variables
    env_vars: HashMap<String, String>,
}

impl ExecHook {
    /// Create a new command execution hook
    ///
    /// # Arguments
    ///
    /// * `command` - Shell command to execute
    /// * `env_vars` - Additional environment variable key-value pairs
    pub fn new(command: String, env_vars: HashMap<String, String>) -> Self {
        Self { command, env_vars }
    }

    /// Build environment variable mapping
    ///
    /// Merges user-defined environment variables with aria2 built-in environment variables.
    pub(crate) fn build_env(
        &self,
        context: &HookContext,
        status_override: Option<&str>,
    ) -> HashMap<String, String> {
        let mut env = HashMap::new();

        // Inject aria2-specific environment variables
        env.insert("ARIA2_GID".to_string(), context.gid.value().to_string());
        env.insert(
            "ARIA2_PATH".to_string(),
            context.file_path.to_string_lossy().to_string(),
        );
        env.insert(
            "ARIA2_STATUS".to_string(),
            status_override
                .unwrap_or(&context.status.to_string())
                .to_string(),
        );
        if let Some(ref err) = context.error {
            env.insert("ARIA2_ERROR".to_string(), err.clone());
        }
        env.insert(
            "ARIA2_DOWNLOADED_BYTES".to_string(),
            context.stats.downloaded_bytes.to_string(),
        );
        env.insert(
            "ARIA2_UPLOADED_BYTES".to_string(),
            context.stats.uploaded_bytes.to_string(),
        );
        env.insert(
            "ARIA2_DOWNLOAD_SPEED".to_string(),
            context.stats.download_speed.to_string(),
        );
        env.insert(
            "ARIA2_UPLOAD_SPEED".to_string(),
            context.stats.upload_speed.to_string(),
        );

        // Merge user-defined environment variables (can override built-in variables)
        for (k, v) in &self.env_vars {
            env.insert(k.clone(), v.clone());
        }

        env
    }
}

#[async_trait]
impl PostDownloadHook for ExecHook {
    /// Execute external command
    ///
    /// Executes the configured command via shell, injecting aria2-related environment variables.
    /// Non-zero exit codes are treated as execution failures.
    async fn on_complete(&self, context: &HookContext) -> Result<()> {
        let env = self.build_env(context, None);

        info!(
            hook = "ExecHook",
            command = %self.command,
            "Executing command on complete"
        );

        let output = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .envs(&env)
            .output()
            .await
            .map_err(|e| {
                Aria2Error::Io(format!(
                    "Failed to execute command '{}': {}",
                    self.command, e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                hook = "ExecHook",
                command = %self.command,
                exit_code = ?output.status.code(),
                stderr = %stderr,
                "Command failed with non-zero exit code"
            );
            return Err(Aria2Error::DownloadFailed(format!(
                "Command '{}' failed with exit code {:?}: {}",
                self.command,
                output.status.code(),
                stderr.trim()
            )));
        }

        debug!(
            hook = "ExecHook",
            stdout = %String::from_utf8_lossy(&output.stdout),
            "Command executed successfully"
        );

        Ok(())
    }

    /// Execute command on download error
    ///
    /// Similar to `on_complete`, but the status is set to "error".
    async fn on_error(&self, context: &HookContext, error: &str) -> Result<()> {
        let mut ctx_with_error = context.clone();
        ctx_with_error.error = Some(error.to_string());
        let env = self.build_env(&ctx_with_error, Some("error"));

        info!(
            hook = "ExecHook",
            command = %self.command,
            "Executing command on error"
        );

        let output = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .envs(&env)
            .output()
            .await
            .map_err(|e| {
                Aria2Error::Io(format!(
                    "Failed to execute command '{}': {}",
                    self.command, e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Aria2Error::DownloadFailed(format!(
                "Command '{}' failed with exit code {:?}: {}",
                self.command,
                output.status.code(),
                stderr.trim()
            )));
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "ExecHook"
    }
}
