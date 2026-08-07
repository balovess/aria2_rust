//! Core types for SFTP download command.
//!
//! Contains the `SftpDownloadCommand` struct definition, constructor,
//! URI parsing, SSH option building, and SFTP/SSH error classification.

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::uri::sftp_path_decode;

use aria2_protocol::sftp::connection::{HostKeyCheckingMode, SshError, SshOptions};
use aria2_protocol::sftp::file_ops::FileOpError;

/// Command that executes an SFTP file download from a remote server to local disk.
///
/// This is the primary integration point between the aria2 download engine and
/// the SFTP protocol layer. It manages the full lifecycle of an SFTP download
/// including connection management, authentication, data transfer, progress tracking,
/// and cleanup.
pub struct SftpDownloadCommand {
    /// The request group that owns this download (tracks state, progress, etc.)
    pub(super) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Local filesystem path where the downloaded file will be written
    pub(super) output_path: std::path::PathBuf,
    /// Whether the command has started executing (prevents double-start)
    pub(super) started: bool,
    /// Total bytes completed so far (for progress tracking)
    pub(super) completed_bytes: u64,
    /// Remote server hostname or IP
    pub(super) host: String,
    /// Remote server port (typically 22)
    pub(super) port: u16,
    /// Username for SSH authentication
    pub(super) username: String,
    /// Password for authentication (optional if using key-based auth)
    pub(super) password: Option<String>,
    /// Expected SSH host-key fingerprint from `--ssh-host-key-md`.
    pub(super) host_key_fingerprint: Option<String>,
    /// Path to the file on the remote SFTP server
    pub(super) remote_path: String,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` for this download.
    pub(super) global_limiter: Option<RateLimiter>,
}

impl SftpDownloadCommand {
    pub(super) fn validate_resume_offset(existing_length: u64, total_length: u64) -> Result<u64> {
        if existing_length > total_length {
            return Err(Aria2Error::FileIo(format!(
                "Local SFTP output is longer than the remote file: {} > {}",
                existing_length, total_length
            )));
        }
        Ok(existing_length)
    }

    /// Create a new SFTP download command.
    ///
    /// # Arguments
    /// * `gid` - Unique group identifier for this download
    /// * `uri` - The sftp:// URI to download from
    /// * `options` - Download configuration options
    /// * `output_dir` - Optional override for output directory
    /// * `output_name` - Optional override for output filename
    ///
    /// # URI Format
    /// ```text
    /// sftp://[user[:password]@]host[:port]/path/to/file
    ///
    /// Examples:
    ///   sftp://user@example.com/path/to/file.txt
    ///   sftp://admin:secret@192.168.1.100:2222/data/archive.tar.gz
    ///   sftp://root@server.example.com:22/etc/config.conf
    /// ```
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        // Step 1: Parse the SFTP URI into components
        let (host, port, username, password, remote_path) = Self::parse_uri(uri)?;

        // Step 2: Determine output directory
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        // Step 3: Determine output filename
        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(&remote_path))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        // Step 4: Build full output path
        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Step 5: Create the request group
        let group = RequestGroup::new(gid, vec![uri.to_string()], options.clone());

        info!(
            "[SFTP-CMD] Created download command: {} -> {} ({}@{}:{}/{})",
            uri,
            path.display(),
            username,
            host,
            port,
            remote_path
        );

        Ok(Self {
            group: Arc::new(std::sync::RwLock::new(group)),
            output_path: path,
            started: false,
            completed_bytes: 0,
            host,
            port,
            username,
            password,
            host_key_fingerprint: options.ssh_host_key_md.clone(),
            remote_path,
            global_limiter: None,
        })
    }

    /// Construct an SFTP command while preserving the engine-owned group.
    #[cfg(feature = "sftp")]
    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let (host, port, username, password, remote_path) = Self::parse_uri(uri)?;
        let dir = output_dir
            .map(str::to_owned)
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());
        let filename = output_name
            .map(str::to_owned)
            .or_else(|| Self::extract_filename(&remote_path))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());
        Ok(Self {
            group,
            output_path: std::path::PathBuf::from(dir).join(filename),
            started: false,
            completed_bytes: 0,
            host,
            port,
            username,
            password,
            host_key_fingerprint: options.ssh_host_key_md.clone(),
            remote_path,
            global_limiter: None,
        })
    }

    /// Parse an sftp:// URI into its component parts.
    ///
    /// Supports the following formats:
    /// - `sftp://user@host/path`
    /// - `sftp://user:password@host/path`
    /// - `sftp://user@host:port/path`
    /// - `sftp://user:password@host:port/path`
    pub(super) fn parse_uri(uri: &str) -> Result<(String, u16, String, Option<String>, String)> {
        if !uri.starts_with("sftp://") {
            return Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol {
                protocol: "sftp".into(),
            }));
        }

        let without_scheme = uri.trim_start_matches("sftp://");

        // Split authority from path
        let (auth_host_port, path) = match without_scheme.find('/') {
            Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
            None => (without_scheme, "/"),
        };

        // Split user info from host:port. A missing user falls back to the
        // local account while preserving the URI's host and remote path.
        let (username, rest) = match auth_host_port.find('@') {
            Some(idx) => (
                auth_host_port[..idx].to_string(),
                &auth_host_port[idx + 1..],
            ),
            None => (
                std::env::var("USER")
                    .or_else(|_| std::env::var("USERNAME"))
                    .unwrap_or_else(|_| "root".to_string()),
                auth_host_port,
            ),
        };

        // Extract optional password from username
        let password = username
            .split_once(':')
            .map(|(_, password)| password.to_string());
        let clean_user = username
            .split_once(':')
            .map_or(username.as_str(), |(user, _)| user)
            .to_string();

        // Split host from port. Bracketed IPv6 literals are handled without
        // mistaking their internal colons for a port separator.
        let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            let (host, suffix) = bracketed
                .split_once(']')
                .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("Invalid SFTP host".into())))?;
            let port = suffix
                .strip_prefix(':')
                .and_then(|value| value.parse().ok())
                .unwrap_or(constants::SFTP_DEFAULT_PORT);
            (host.to_string(), port)
        } else {
            match rest.rsplit_once(':') {
                Some((host, port)) if port.parse::<u16>().is_ok() => {
                    (host.to_string(), port.parse().unwrap())
                }
                _ => (rest.to_string(), constants::SFTP_DEFAULT_PORT),
            }
        };

        Ok((host, port, clean_user, password, sftp_path_decode(path)))
    }

    /// Extract the filename component from a remote path.
    pub(super) fn extract_filename(remote_path: &str) -> Option<String> {
        remote_path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "/")
            .map(|s| s.to_string())
    }

    /// Get a read-only reference to the request group.
    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, the disk writer created in `execute` acquires tokens from this
    /// limiter in addition to the per-download limiter.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
    }

    /// Build SshOptions from the command's stored credentials.
    pub(super) fn build_ssh_options(&self) -> SshOptions {
        let mut opts = SshOptions::new(&self.host, &self.username)
            .with_port(self.port)
            .with_timeouts(
                Duration::from_secs(constants::SFTP_CONNECT_TIMEOUT_SECS),
                Duration::from_secs(constants::SFTP_READ_TIMEOUT_SECS),
            )
            .with_host_key_mode(if self.host_key_fingerprint.is_some() {
                HostKeyCheckingMode::Strict
            } else {
                HostKeyCheckingMode::AcceptNew
            });
        if let Some(fingerprint) = self.host_key_fingerprint.as_deref() {
            opts = opts.with_host_key_fingerprint(fingerprint);
        }

        if let Some(ref pwd) = self.password {
            opts = opts.with_password(pwd);
        }

        opts
    }

    /// Map an SshError to the appropriate Aria2Error for engine-level handling.
    pub(super) fn map_ssh_error(err: &SshError, host: &str, port: u16, _path: &str) -> Aria2Error {
        match err {
            SshError::AuthFailed { .. } => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("{}:{}", host, port),
            }),
            SshError::ConnectTimeout { .. } | SshError::ConnectFailed { .. } => {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: err.to_string(),
                })
            }
            SshError::Handshake { .. } | SshError::ConnectionLost { .. } => {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: err.to_string(),
                })
            }
            SshError::NoCredentials { .. } => Aria2Error::Fatal(FatalError::Config(format!(
                "No SSH credentials provided for {}:{}",
                host, port
            ))),
            _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("SFTP error [{}:{}]: {}", host, port, err),
            }),
        }
    }

    /// Map a FileOpError to the appropriate Aria2Error for engine-level handling.
    pub(super) fn map_file_op_error(err: &FileOpError, host: &str, path: &str) -> Aria2Error {
        match err {
            FileOpError::NotFound { .. } => Aria2Error::Fatal(FatalError::FileNotFound {
                path: path.to_string(),
            }),
            FileOpError::PermissionDenied { .. } => {
                Aria2Error::Fatal(FatalError::PermissionDenied {
                    path: format!("{}:{}", host, path),
                })
            }
            FileOpError::Network { .. } => {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: err.to_string(),
                })
            }
            _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("SFTP file op error: {}", err),
            }),
        }
    }
}
