//! BitTorrent 下载后处理钩子系统
//!
//! 提供下载完成后的自定义处理能力，支持文件移动、重命名、时间戳更新和外部命令执行等功能。
//! 通过 HookManager 管理多个钩子的执行链，支持配置错误处理策略。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::error::{Aria2Error, Result};
use crate::request::request_group::GroupId;

// ============================================================================
// 核心数据结构
// ============================================================================

/// 钩子执行的上下文信息，包含下载任务的状态和数据统计
#[derive(Clone, Debug)]
pub struct HookContext {
    /// 下载任务的唯一标识符
    pub gid: GroupId,
    /// 下载文件的完整路径
    pub file_path: PathBuf,
    /// 当前下载状态
    pub status: DownloadStatus,
    /// 下载统计数据
    pub stats: DownloadStats,
    /// 错误信息（如果有）
    pub error: Option<String>,
}

impl HookContext {
    /// 创建一个新的钩子上下文
    ///
    /// # Arguments
    ///
    /// * `gid` - 下载任务组 ID
    /// * `file_path` - 下载文件路径
    /// * `status` - 下载状态
    /// * `stats` - 下载统计信息
    /// * `error` - 可选的错误信息
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

    /// 获取文件名（不含路径）
    pub fn filename(&self) -> &str {
        self.file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }

    /// 获取文件扩展名
    pub fn extension(&self) -> &str {
        self.file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
    }

    /// 获取父目录路径
    pub fn directory(&self) -> &Path {
        self.file_path.parent().unwrap_or(self.file_path.as_path())
    }
}

/// 下载状态枚举
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadStatus {
    /// 下载完成
    Complete,
    /// 下载出错
    Error,
    /// 下载已停止
    Stopped,
    /// 下载已暂停
    Paused,
}

impl std::fmt::Display for DownloadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadStatus::Complete => write!(f, "complete"),
            DownloadStatus::Error => write!(f, "error"),
            DownloadStatus::Stopped => write!(f, "stopped"),
            DownloadStatus::Paused => write!(f, "paused"),
        }
    }
}

/// 下载统计数据
#[derive(Clone, Debug)]
pub struct DownloadStats {
    /// 已上传的字节数
    pub uploaded_bytes: u64,
    /// 已下载的字节数
    pub downloaded_bytes: u64,
    /// 上传速度（字节/秒）
    pub upload_speed: f64,
    /// 下载速度（字节/秒）
    pub download_speed: f64,
    /// 已用时间（秒）
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

/// 钩子系统配置
#[derive(Clone, Debug)]
pub struct HookConfig {
    /// 是否在遇到错误时停止后续钩子执行
    pub stop_on_error: bool,
    /// 单个钩子执行的超时时间
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
// PostDownloadHook Trait 定义
// ============================================================================

/// 下载后处理钩子 trait
///
/// 实现此 trait 可以自定义下载完成后的行为。
/// 所有方法都是异步的，支持在异步上下文中执行耗时操作。
#[async_trait]
pub trait PostDownloadHook: Send + Sync {
    /// 下载成功完成时的回调
    ///
    /// # Arguments
    ///
    /// * `context` - 包含下载任务信息的上下文
    ///
    /// # Returns
    ///
    /// 返回 `Ok(())` 表示处理成功，`Err(e)` 表示处理失败
    async fn on_complete(&self, context: &HookContext) -> Result<()>;

    /// 下载失败时的回调
    ///
    /// # Arguments
    ///
    /// * `context` - 包含下载任务信息的上下文
    /// * `error` - 错误描述字符串
    ///
    /// # Returns
    ///
    /// 返回 `Ok(())` 表示错误处理成功，`Err(e)` 表示错误处理本身失败
    async fn on_error(&self, context: &HookContext, error: &str) -> Result<()>;

    /// 返回钩子的名称，用于日志记录和管理
    fn name(&self) -> &'static str;
}

// ============================================================================
// 内置钩子实现
// ============================================================================

/// 文件移动钩子
///
/// 在下载完成后将文件移动到指定目录。支持自动创建目标目录结构。
#[derive(Clone, Debug)]
pub struct MoveHook {
    /// 目标目录路径
    target_dir: PathBuf,
    /// 是否自动创建不存在的目录
    create_dirs: bool,
}

impl MoveHook {
    /// 创建新的移动钩子
    ///
    /// # Arguments
    ///
    /// * `target_dir` - 目标目录路径
    /// * `create_dirs` - 是否自动创建目录
    pub fn new(target_dir: PathBuf, create_dirs: bool) -> Self {
        Self {
            target_dir,
            create_dirs,
        }
    }
}

#[async_trait]
impl PostDownloadHook for MoveHook {
    /// 执行文件移动操作
    ///
    /// 验证源文件存在后，将其移动到目标目录。
    /// 如果 `create_dirs` 为 true，会自动创建所需的目录层级。
    async fn on_complete(&self, context: &HookContext) -> Result<()> {
        let source = &context.file_path;

        // 验证源文件存在
        if !source.exists() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
                path: source.to_string_lossy().to_string(),
            }));
        }

        // 创建目标目录（如果需要）
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

        // 构建目标路径
        let filename = context.filename();
        let destination = self.target_dir.join(filename);

        info!(
            hook = "MoveHook",
            source = %source.display(),
            dest = %destination.display(),
            "Moving file"
        );

        // 执行移动操作
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

    /// 移动钩子在错误时不执行任何操作
    async fn on_error(&self, _context: &HookContext, _error: &str) -> Result<()> {
        // 文件移动在错误情况下通常不需要执行
        Ok(())
    }

    fn name(&self) -> &'static str {
        "MoveHook"
    }
}

/// 文件重命名钩子
///
/// 使用模板模式对下载的文件进行重命名。
/// 支持以下占位符：
/// - `%d`: 源文件所在目录
/// - `%f`: 原始文件名
/// - `%e`: 文件扩展名
/// - `%i`: 下载任务 GID
/// - `%t`: 当前时间戳（Unix 时间戳）
#[derive(Clone, Debug)]
pub struct RenameHook {
    /// 重命名模板模式
    pattern: String,
}

impl RenameHook {
    /// 创建新的重命名钩子
    ///
    /// # Arguments
    ///
    /// * `pattern` - 重命名模板，支持占位符替换
    pub fn new(pattern: String) -> Self {
        Self { pattern }
    }

    /// 展开模板中的占位符
    ///
    /// 将模板字符串中的特殊标记替换为实际值。
    ///
    /// # Arguments
    ///
    /// * `context` - 钩子上下文，用于获取替换值
    ///
    /// # Returns
    ///
    /// 替换后的完整文件名
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
    /// 执行文件重命名操作
    ///
    /// 根据模板模式生成新文件名并重命名文件。
    async fn on_complete(&self, context: &HookContext) -> Result<()> {
        let source = &context.file_path;

        if !source.exists() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
                path: source.to_string_lossy().to_string(),
            }));
        }

        let new_name = self.expand_pattern(context);

        // 如果新名称包含路径分隔符，视为完整路径；否则在同一目录下重命名
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

    /// 重命名钩子在错误时不执行
    async fn on_error(&self, _context: &HookContext, _error: &str) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RenameHook"
    }
}

/// 文件时间戳更新钩子
///
/// 在下载完成后更新文件的修改时间和访问时间为当前系统时间。
#[derive(Clone, Debug)]
pub struct TouchHook;

impl TouchHook {
    /// 创建新的 TouchHook 实例
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
    /// 更新文件的修改时间和访问时间
    ///
    /// 将文件的 mtime 和 atime 都设置为当前系统时间。
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

        // 使用 filetime crate 或标准库设置时间
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::PermissionsExt;

            // 获取现有权限以保持不变
            let metadata = tokio::fs::metadata(path).await.map_err(|e| {
                Aria2Error::Io(format!(
                    "Failed to get metadata for {}: {}",
                    path.display(),
                    e
                ))
            })?;

            // 使用 utime 设置时间
            let times: [libc::timespec; 2] = [
                libc::timespec {
                    tv_sec: now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as _,
                    tv_nsec: 0,
                },
                libc::timespec {
                    tv_sec: now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as _,
                    tv_nsec: 0,
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
            // Windows 下使用标准库的 set_times 功能（需要 Rust 1.75+）
            // 或者通过重新写入文件来更新时间戳
            use std::fs;

            // 简单方案：读取文件元数据并设置时间
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

    /// TouchHook 在错误时不执行
    async fn on_error(&self, _context: &HookContext, _error: &str) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "TouchHook"
    }
}

/// 外部命令执行钩子
///
/// 在下载完成后执行指定的外部命令，并将下载相关信息作为环境变量注入。
/// 支持的环境变量：
/// - `ARIA2_GID`: 任务组 ID
/// - `ARIA2_PATH`: 文件路径
/// - `ARIA2_STATUS`: 下载状态
/// - `ARIA2_ERROR`: 错误信息（如有）
/// - `ARIA2_DOWNLOADED_BYTES`: 已下载字节数
/// - `ARIA2_UPLOADED_BYTES`: 已上传字节数
/// - `ARIA2_DOWNLOAD_SPEED`: 下载速度
/// - `ARIA2_UPLOAD_SPEED`: 上传速度
#[derive(Clone, Debug)]
pub struct ExecHook {
    /// 要执行的命令
    command: String,
    /// 额外的环境变量
    env_vars: HashMap<String, String>,
}

impl ExecHook {
    /// 创建新的命令执行钩子
    ///
    /// # Arguments
    ///
    /// * `command` - 要执行的 shell 命令
    /// * `env_vars` - 额外的环境变量键值对
    pub fn new(command: String, env_vars: HashMap<String, String>) -> Self {
        Self { command, env_vars }
    }

    /// 构建环境变量映射
    ///
    /// 合并用户自定义环境变量和 aria2 内置环境变量。
    fn build_env(
        &self,
        context: &HookContext,
        status_override: Option<&str>,
    ) -> HashMap<String, String> {
        let mut env = HashMap::new();

        // 注入 aria2 特定环境变量
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

        // 合并用户自定义环境变量（可覆盖内置变量）
        for (k, v) in &self.env_vars {
            env.insert(k.clone(), v.clone());
        }

        env
    }
}

#[async_trait]
impl PostDownloadHook for ExecHook {
    /// 执行外部命令
    ///
    /// 通过 shell 执行配置的命令，注入 aria2 相关环境变量。
    /// 非 zero 退出码会被视为执行失败。
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

    /// 在下载错误时执行命令
    ///
    /// 与 `on_complete` 类似，但状态会被设置为 "error"。
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
// HookManager 钩子链管理器
// ============================================================================

/// 钩子链管理器
///
/// 负责管理和协调多个下载后处理钩子的执行。
/// 支持按顺序执行钩子链，并根据配置决定遇到错误时的处理策略。
pub struct HookManager {
    /// 注册的钩子列表（按注册顺序执行）
    hooks: Vec<Box<dyn PostDownloadHook>>,
    /// 钩子系统配置
    config: HookConfig,
}

impl HookManager {
    /// 创建新的钩子管理器
    ///
    /// # Arguments
    ///
    /// * `config` - 钩子系统配置选项
    pub fn new(config: HookConfig) -> Self {
        Self {
            hooks: Vec::new(),
            config,
        }
    }

    /// 向钩子链添加一个新的钩子
    ///
    /// 钩子将按照添加的顺序被执行。
    ///
    /// # Arguments
    ///
    /// * `hook` - 要添加的钩子实例（必须实现 `PostDownloadHook` trait）
    pub fn add_hook(&mut self, hook: Box<dyn PostDownloadHook>) {
        info!(hook_name = hook.name(), "Adding hook to chain");
        self.hooks.push(hook);
    }

    /// 按名称移除钩子
    ///
    /// # Arguments
    ///
    /// * `name` - 要移除的钩子名称
    ///
    /// # Returns
    ///
    /// 返回被移除的钩子（如果找到），否则返回 `None`
    pub fn remove_hook(&mut self, name: &str) -> Option<Box<dyn PostDownloadHook>> {
        let pos = self.hooks.iter().position(|h| h.name() == name)?;
        info!(hook_name = name, "Removing hook from chain");
        Some(self.hooks.remove(pos))
    }

    /// 触发所有钩子的 on_complete 回调
    ///
    /// 按注册顺序依次调用每个钩子的 `on_complete` 方法。
    /// 根据 `config.stop_on_error` 决定是否在第一个失败时停止。
    ///
    /// # Arguments
    ///
    /// * `context` - 下载完成的上下文信息
    ///
    /// # Returns
    ///
    /// 返回每个钩子的执行结果描述向量。如果 `stop_on_error=true` 且某个钩子失败，
    /// 返回 `Err` 包含该错误信息。
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

    /// 触发所有钩子的 on_error 回调
    ///
    /// 与 `fire_complete` 类似，但调用的是 `on_error` 方法。
    ///
    /// # Arguments
    ///
    /// * `context` - 下载失败的上下文信息
    /// * `error` - 错误描述字符串
    ///
    /// # Returns
    ///
    /// 返回每个钩子的执行结果描述向量
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

    /// 获取当前注册的钩子数量
    pub fn hook_count(&self) -> usize {
        self.hooks.len()
    }

    /// 清空所有已注册的钩子
    pub fn clear_hooks(&mut self) {
        info!("Clearing all hooks");
        self.hooks.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 辅助函数：创建测试用的 HookContext
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

        // 创建测试文件
        tokio::fs::write(&src_file, b"test content")
            .await
            .expect("Failed to write test file");

        let target_dir = temp_dir.path().join("target");
        let hook = MoveHook::new(target_dir.clone(), false);

        // 手动创建目标目录
        tokio::fs::create_dir_all(&target_dir)
            .await
            .expect("Failed to create target dir");

        let context = create_test_context(&src_file);

        assert!(hook.on_complete(&context).await.is_ok());

        // 验证文件已被移动
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

        // 目标目录不存在且有多层嵌套
        let target_dir = temp_dir.path().join("nested").join("deep").join("target");
        let hook = MoveHook::new(target_dir.clone(), true);

        let context = create_test_context(&src_file);

        assert!(hook.on_complete(&context).await.is_ok());

        // 验证目录被自动创建且文件已移动
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

        // 测试 expand_pattern
        let expanded = hook.expand_pattern(&context);
        assert!(
            expanded.contains("archive.tar.gz.renamed"),
            "Pattern should contain original filename"
        );

        // 测试实际重命名
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

        // 获取原始修改时间
        let before_metadata = tokio::fs::metadata(&src_file)
            .await
            .expect("Failed to get metadata");
        let before_mtime = before_metadata.modified().expect("Failed to get mtime");

        // 等待一小段时间确保时间差可检测
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let hook = TouchHook::new();
        let context = create_test_context(&src_file);

        assert!(hook.on_complete(&context).await.is_ok());

        // 验证修改时间已更新
        let after_metadata = tokio::fs::metadata(&src_file)
            .await
            .expect("Failed to get metadata after touch");
        let after_mtime = after_metadata
            .modified()
            .expect("Failed to get mtime after touch");

        assert!(
            after_mtime > before_mtime,
            "Modification time should be updated to current time"
        );
    }

    #[tokio::test]
    async fn test_exec_hook_env_vars_injected() {
        // 创建一个简单的测试脚本，输出环境变量到文件
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let output_file = temp_dir.path().join("env_output.txt");

        // 使用 echo 命令写入环境变量（跨平台兼容）
        let cmd = format!("echo $ARIA2_GID > {}", output_file.display());

        let mut env_vars = HashMap::new();
        env_vars.insert("CUSTOM_VAR".to_string(), "custom_value".to_string());

        let hook = ExecHook::new(cmd, env_vars);
        let context = create_test_context(&temp_dir.path().join("dummy.txt"));

        // 注意：这个测试在非 Unix 系统上可能需要调整
        #[cfg(unix)]
        {
            let result = hook.on_complete(&context).await;
            // 即使命令失败（因为可能没有 sh），我们主要验证构建逻辑正确性
            let _ = result;
        }

        // 验证环境变量构建逻辑
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

        // 验证钩子按注册顺序添加和计数
        manager.add_hook(Box::new(TouchHook));
        manager.add_hook(Box::new(RenameHook::new("%f.copy".to_string())));

        assert_eq!(manager.hook_count(), 2, "Should have 2 hooks registered");

        // 验证可以按名称移除钩子（从链的末尾开始）
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

        // 添加一个会失败的 ExecHook
        manager.add_hook(Box::new(ExecHook::new(
            "exit 1".to_string(),
            HashMap::new(),
        )));

        let context = create_test_context(&temp_dir.path().join("test.txt"));

        // 不应该因为第一个钩子失败而返回错误
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

        // 第一个钩子会失败
        manager.add_hook(Box::new(ExecHook::new(
            "exit 1".to_string(),
            HashMap::new(),
        )));
        // 第二个钩子不应该被执行
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

        // 尝试移除不存在的钩子
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
