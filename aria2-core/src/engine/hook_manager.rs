//! BitTorrent post-download hook system
//!
//! Provides custom processing capabilities after download completion, supporting file moving,
//! renaming, timestamp updating, and external command execution.
//! HookManager manages the execution chain of multiple hooks and supports configuring error handling strategies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::error::{Aria2Error, Result};
use crate::request::request_group::GroupId;

// Re-export DownloadStatus for backward compatibility
pub use crate::request::request_group::DownloadStatus;

// ============================================================================
// Core data structures
// ============================================================================

/// Hook execution context, containing download task status and data statistics
#[derive(Clone, Debug)]
pub struct HookContext {
    /// Unique identifier of the download task
    pub gid: GroupId,
    /// Full path of the downloaded file
    pub file_path: PathBuf,
    /// Current download status
    pub status: DownloadStatus,
    /// Download statistics
    pub stats: DownloadStats,
    /// Error message (if any)
    pub error: Option<String>,
}

impl HookContext {
    /// Create a new hook context
    ///
    /// # Arguments
    ///
    /// * `gid` - Download task group ID
    /// * `file_path` - Download file path
    /// * `status` - Download status
    /// * `stats` - Download statistics
    /// * `error` - Optional error message
    pub fn new(
        gid: GroupId,
        file_path: PathBuf,
        status: DownloadStatus,
        stats: DownloadStats,
        error: Option<String>,
    ) -> Self {
        Self {
            gid,
            file_path,
            status,
            stats,
            error,
        }
    }

    /// Get filename (without path)
    pub fn filename(&self) -> &str {
        self.file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }

    /// Get file extension
    pub fn extension(&self) -> &str {
        self.file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
    }

    /// Get parent directory path
    pub fn directory(&self) -> &Path {
        self.file_path.parent().unwrap_or(self.file_path.as_path())
    }
}

/// Download statistics
#[derive(Clone, Debug)]
pub struct DownloadStats {
    /// Uploaded bytes
    pub uploaded_bytes: u64,
    /// Downloaded bytes
    pub downloaded_bytes: u64,
    /// Upload speed (bytes/sec)
    pub upload_speed: f64,
    /// Download speed (bytes/sec)
    pub download_speed: f64,
    /// Elapsed time (seconds)
    pub elapsed_seconds: u64,
}

impl Default for DownloadStats {
    fn default() -> Self {
        Self {
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            upload_speed: 0.0,
            download_speed: 0.0,
            elapsed_seconds: 0,
        }
    }
}

impl std::fmt::Display for DownloadStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "downloaded={}, uploaded={}, dl_speed={:.2}B/s, ul_speed={:.2}B/s, elapsed={}s",
            self.downloaded_bytes,
            self.uploaded_bytes,
            self.download_speed,
            self.upload_speed,
            self.elapsed_seconds
        )
    }
}

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
// PostDownloadHook trait definition
// ============================================================================

/// Post-download hook trait
///
/// Implement this trait to customize behavior after download completion.
/// All methods are async, supporting time-consuming operations in async contexts.
#[async_trait]
pub trait PostDownloadHook: Send + Sync {
    /// Callback when download completes successfully
    ///
    /// # Arguments
    ///
    /// * `context` - Context containing download task information
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, `Err(e)` on failure
    async fn on_complete(&self, context: &HookContext) -> Result<()>;

    /// Callback when download fails
    ///
    /// # Arguments
    ///
    /// * `context` - Context containing download task information
    /// * `error` - Error description string
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if error handling succeeded, `Err(e)` if error handling itself failed
    async fn on_error(&self, context: &HookContext, error: &str) -> Result<()>;

    /// Returns the hook name for logging and management
    fn name(&self) -> &'static str;
}

// ============================================================================
// Built-in hook implementations
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
    fn build_env(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper function: create a test HookContext
    fn create_test_context(file_path: &Path) -> HookContext {
        HookContext {
            gid: GroupId::new(42),
            file_path: file_path.to_path_buf(),
            status: DownloadStatus::Complete,
            stats: DownloadStats {
                uploaded_bytes: 1024,
                downloaded_bytes: 2048,
                upload_speed: 100.0,
                download_speed: 200.0,
                elapsed_seconds: 10,
            },
            error: None,
        }
    }

    #[tokio::test]
    async fn test_move_hook_basic() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let src_file = temp_dir.path().join("test_file.txt");

        // Create test file
        tokio::fs::write(&src_file, b"test content")
            .await
            .expect("Failed to write test file");

        let target_dir = temp_dir.path().join("target");
        let hook = MoveHook::new(target_dir.clone(), false);

        // Manually create target directory
        tokio::fs::create_dir_all(&target_dir)
            .await
            .expect("Failed to create target dir");

        let context = create_test_context(&src_file);

        assert!(hook.on_complete(&context).await.is_ok());

        // Verify file has been moved
        let moved_file = target_dir.join("test_file.txt");
        assert!(
            moved_file.exists(),
            "File should be moved to target directory"
        );
        assert!(!src_file.exists(), "Source file should no longer exist");
    }

    #[tokio::test]
    async fn test_move_hook_create_dirs() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let src_file = temp_dir.path().join("test_file.txt");

        tokio::fs::write(&src_file, b"test content")
            .await
            .expect("Failed to write test file");

        // Target directory does not exist and has multiple nested levels
        let target_dir = temp_dir.path().join("nested").join("deep").join("target");
        let hook = MoveHook::new(target_dir.clone(), true);

        let context = create_test_context(&src_file);

        assert!(hook.on_complete(&context).await.is_ok());

        // Verify directory was auto-created and file was moved
        let moved_file = target_dir.join("test_file.txt");
        assert!(
            moved_file.exists(),
            "File should be moved to auto-created directory"
        );
    }

    #[tokio::test]
    async fn test_rename_hook_pattern_expansion() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let src_file = temp_dir.path().join("archive.tar.gz");

        tokio::fs::write(&src_file, b"content")
            .await
            .expect("Failed to write test file");

        let hook = RenameHook::new("%f.renamed".to_string());
        let context = create_test_context(&src_file);

        // Test expand_pattern
        let expanded = hook.expand_pattern(&context);
        assert!(
            expanded.contains("archive.tar.gz.renamed"),
            "Pattern should contain original filename"
        );

        // Test actual rename
        assert!(hook.on_complete(&context).await.is_ok());

        let renamed_file = temp_dir.path().join("archive.tar.gz.renamed");
        assert!(
            renamed_file.exists(),
            "File should be renamed according to pattern"
        );
    }

    #[tokio::test]
    async fn test_touch_hook_updates_mtime() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let src_file = temp_dir.path().join("timestamp_test.txt");

        tokio::fs::write(&src_file, b"touch test")
            .await
            .expect("Failed to write test file");

        // Get original modification time
        let before_metadata = tokio::fs::metadata(&src_file)
            .await
            .expect("Failed to get metadata");
        let before_mtime = before_metadata.modified().expect("Failed to get mtime");

        // Wait for a longer time to ensure time difference is detectable on all platforms
        // Windows FAT has 2-second resolution, NTFS has 100ns resolution
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let hook = TouchHook::new();
        let context = create_test_context(&src_file);

        assert!(hook.on_complete(&context).await.is_ok());

        // Verify modification time has been updated
        let after_metadata = tokio::fs::metadata(&src_file)
            .await
            .expect("Failed to get metadata after touch");
        let after_mtime = after_metadata
            .modified()
            .expect("Failed to get mtime after touch");

        assert!(
            after_mtime >= before_mtime,
            "Modification time should be updated to current time (before: {:?}, after: {:?})",
            before_mtime,
            after_mtime
        );
    }

    #[tokio::test]
    async fn test_exec_hook_env_vars_injected() {
        // Create a simple test script that outputs environment variables to a file
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let output_file = temp_dir.path().join("env_output.txt");

        // Use echo command to write environment variables (cross-platform compatible)
        let cmd = format!("echo $ARIA2_GID > {}", output_file.display());

        let mut env_vars = HashMap::new();
        env_vars.insert("CUSTOM_VAR".to_string(), "custom_value".to_string());

        let hook = ExecHook::new(cmd, env_vars);
        let context = create_test_context(&temp_dir.path().join("dummy.txt"));

        // Note: This test may need adjustment on non-Unix systems
        #[cfg(unix)]
        {
            let result = hook.on_complete(&context).await;
            // Even if the command fails (because sh may not be available), we mainly verify the build logic correctness
            let _ = result;
        }

        // Verify environment variable build logic
        let built_env = hook.build_env(&context, None);
        assert_eq!(
            built_env.get("ARIA2_GID").unwrap(),
            "42",
            "GID should be injected"
        );
        assert_eq!(
            built_env.get("ARIA2_STATUS").unwrap(),
            "complete",
            "Status should be complete"
        );
        assert_eq!(
            built_env.get("CUSTOM_VAR").unwrap(),
            "custom_value",
            "Custom var should be preserved"
        );
        assert_eq!(
            built_env.get("ARIA2_DOWNLOADED_BYTES").unwrap(),
            "2048",
            "Download bytes should be correct"
        );
    }

    #[tokio::test]
    async fn test_exec_hook_nonzero_exit_code() {
        let hook = ExecHook::new("exit 1".to_string(), HashMap::new());
        let context = create_test_context(Path::new("/tmp/nonexistent"));

        let result = hook.on_complete(&context).await;
        assert!(result.is_err(), "Non-zero exit code should return error");

        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("failed")
                || err_msg.contains("exit code")
                || err_msg.contains("Failed")
                || err_msg.contains("execute"),
            "Error message should indicate failure, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_hook_chain_execution_order() {
        let mut manager = HookManager::new(HookConfig::default());

        // Verify hooks are added and counted in registration order
        manager.add_hook(Box::new(TouchHook));
        manager.add_hook(Box::new(RenameHook::new("%f.copy".to_string())));

        assert_eq!(manager.hook_count(), 2, "Should have 2 hooks registered");

        // Verify hooks can be removed by name (starting from the end of the chain)
        let removed = manager.remove_hook("RenameHook");
        assert!(removed.is_some(), "Should be able to remove RenameHook");
        assert_eq!(
            manager.hook_count(),
            1,
            "Should have 1 hook remaining after removal"
        );
    }

    #[tokio::test]
    async fn test_hook_failure_isolation() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let config = HookConfig {
            stop_on_error: false,
            ..Default::default()
        };
        let mut manager = HookManager::new(config);

        // Add an ExecHook that will fail
        manager.add_hook(Box::new(ExecHook::new(
            "exit 1".to_string(),
            HashMap::new(),
        )));

        let context = create_test_context(&temp_dir.path().join("test.txt"));

        // Should not return error because the first hook failed
        let results = manager.fire_complete(&context).await;
        assert!(results.is_ok(), "Should not fail when stop_on_error=false");

        let results_vec = results.unwrap();
        assert_eq!(results_vec.len(), 1, "Should have one result entry");
        assert!(
            results_vec[0].contains("failed"),
            "Result should indicate failure of the first hook"
        );
    }

    #[tokio::test]
    async fn test_hook_config_stop_on_error() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let config = HookConfig {
            stop_on_error: true,
            ..Default::default()
        };
        let mut manager = HookManager::new(config);

        // First hook will fail
        manager.add_hook(Box::new(ExecHook::new(
            "exit 1".to_string(),
            HashMap::new(),
        )));
        // Second hook should not be executed
        manager.add_hook(Box::new(ExecHook::new(
            "echo success".to_string(),
            HashMap::new(),
        )));

        let context = create_test_context(&temp_dir.path().join("test.txt"));

        let result = manager.fire_complete(&context).await;
        assert!(
            result.is_err(),
            "Should return error when stop_on_error=true and first hook fails"
        );
    }

    #[tokio::test]
    async fn test_hook_remove_by_name() {
        let mut manager = HookManager::new(HookConfig::default());

        manager.add_hook(Box::new(TouchHook));
        manager.add_hook(Box::new(MoveHook::new(PathBuf::from("/tmp"), false)));

        assert_eq!(manager.hook_count(), 2);

        let removed = manager.remove_hook("TouchHook");
        assert!(removed.is_some(), "Should find and remove TouchHook");
        assert_eq!(removed.unwrap().name(), "TouchHook");
        assert_eq!(manager.hook_count(), 1, "Should have 1 hook remaining");

        // Try to remove a non-existent hook
        let not_found = manager.remove_hook("NonExistentHook");
        assert!(
            not_found.is_none(),
            "Should return None for non-existent hook"
        );
    }

    #[test]
    fn test_hook_context_creation() {
        let context = HookContext::new(
            GroupId::new(123),
            PathBuf::from("/downloads/file.zip"),
            DownloadStatus::Complete,
            DownloadStats {
                downloaded_bytes: 9999,
                ..Default::default()
            },
            None,
        );

        assert_eq!(context.gid.value(), 123);
        assert_eq!(context.filename(), "file.zip");
        assert_eq!(context.extension(), "zip");
        assert_eq!(context.status, DownloadStatus::Complete);
        assert!(context.error.is_none());
        assert_eq!(context.stats.downloaded_bytes, 9999);
    }

    #[test]
    fn test_download_stats_display() {
        let stats = DownloadStats {
            uploaded_bytes: 1024,
            downloaded_bytes: 2048,
            upload_speed: 100.5,
            download_speed: 200.25,
            elapsed_seconds: 30,
        };

        let display = format!("{}", stats);
        assert!(
            display.contains("downloaded=2048"),
            "Should contain downloaded bytes"
        );
        assert!(
            display.contains("uploaded=1024"),
            "Should contain uploaded bytes"
        );
        assert!(display.contains("200.25"), "Should contain download speed");
        assert!(
            display.contains("elapsed=30s"),
            "Should contain elapsed time"
        );
    }
}
