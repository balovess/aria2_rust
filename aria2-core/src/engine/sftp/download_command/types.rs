//! Core types for SFTP download command.
//!
//! Contains the `SftpDownloadCommand` struct definition, constructor,
//! URI parsing, SSH option building, and SFTP/SSH error classification.

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::constants;
use crate::engine::progress_checkpoint::ProgressCheckpoint;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::http::auth::netrc::find_netrc_file;
use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::network::OutboundNetworkPolicy;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use aria2_protocol::sftp::connection::{HostKeyCheckingMode, SshError, SshOptions};
use aria2_protocol::sftp::file_ops::FileOpError;
use aria2_protocol::sftp::uri::SftpUriError;

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
    /// Retry policy for transient transport and remote not-found responses.
    pub(super) retry_policy: RetryPolicy,
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
    pub(super) checkpoint: Option<ProgressCheckpoint>,
    /// Process-wide policy used to create the SSH TCP stream.
    pub(super) outbound_network_policy: Arc<OutboundNetworkPolicy>,
}

/// Source-compatible SFTP URI fields before credential resolution.
///
/// `aria2_original` parses URI userinfo separately from its auth resolution
/// chain. Keeping that distinction allows the core `AuthConfigFactory` to
/// apply the same URL, netrc, CLI, and anonymous-default precedence.
pub(super) use aria2_protocol::sftp::uri::SftpUri as ParsedSftpUri;

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
        let parsed = Self::parse_uri(uri)?;
        let (username, password) = Self::resolve_credentials(&parsed, options)?;

        // Step 2: Determine output directory
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        // Step 3: Determine output filename
        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(&parsed.remote_path))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        // Step 4: Build full output path
        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Step 5: Create the request group
        let group = RequestGroup::new(gid, vec![uri.to_string()], options.clone());
        if options.uses_memory_download_for_uri(uri) {
            group.mark_in_memory_download();
        }

        info!(
            "[SFTP-CMD] Created download command: {} -> {} ({}@{}:{}/{})",
            uri,
            path.display(),
            username,
            parsed.host,
            parsed.port,
            parsed.remote_path
        );

        Ok(Self {
            group: Arc::new(std::sync::RwLock::new(group)),
            output_path: path,
            started: false,
            completed_bytes: 0,
            retry_policy: RetryPolicy::new(
                options.max_retries,
                options.retry_wait.saturating_mul(1000),
            ),
            host: parsed.host,
            port: parsed.port,
            username,
            password,
            host_key_fingerprint: options.ssh_host_key_md.clone(),
            remote_path: parsed.remote_path,
            global_limiter: None,
            checkpoint: None,
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
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
        let parsed = Self::parse_uri(uri)?;
        let (username, password) = Self::resolve_credentials(&parsed, options)?;
        if options.uses_memory_download_for_uri(uri) {
            group.recover().mark_in_memory_download();
        }
        let dir = output_dir
            .map(str::to_owned)
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());
        let filename = output_name
            .map(str::to_owned)
            .or_else(|| Self::extract_filename(&parsed.remote_path))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());
        Ok(Self {
            group,
            output_path: std::path::PathBuf::from(dir).join(filename),
            started: false,
            completed_bytes: 0,
            retry_policy: RetryPolicy::new(
                options.max_retries,
                options.retry_wait.saturating_mul(1000),
            ),
            host: parsed.host,
            port: parsed.port,
            username,
            password,
            host_key_fingerprint: options.ssh_host_key_md.clone(),
            remote_path: parsed.remote_path,
            global_limiter: None,
            checkpoint: None,
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
        })
    }

    pub fn set_outbound_network_policy(&mut self, policy: Arc<OutboundNetworkPolicy>) {
        self.outbound_network_policy = policy;
    }

    /// Parse an sftp:// URI into its component parts.
    ///
    /// Supports the following formats:
    /// - `sftp://user@host/path`
    /// - `sftp://user:password@host/path`
    /// - `sftp://user@host:port/path`
    /// - `sftp://user:password@host:port/path`
    pub(super) fn parse_uri(uri: &str) -> Result<ParsedSftpUri> {
        aria2_protocol::sftp::uri::parse_uri(uri).map_err(|error| match error {
            SftpUriError::UnsupportedScheme => Aria2Error::Fatal(FatalError::UnsupportedProtocol {
                protocol: "sftp".into(),
            }),
            SftpUriError::Invalid(message) => {
                Aria2Error::Fatal(FatalError::Config(message.to_string()))
            }
        })
    }

    fn invalid_uri(message: &str) -> Aria2Error {
        Aria2Error::Fatal(FatalError::Config(message.to_string()))
    }

    fn resolve_credentials(
        parsed: &ParsedSftpUri,
        options: &DownloadOptions,
    ) -> Result<(String, Option<String>)> {
        let mut auth_url =
            url::Url::parse("sftp://localhost/").expect("static SFTP URL must be valid");
        let auth_host = if parsed.host.contains(':') {
            format!("[{}]", parsed.host)
        } else {
            parsed.host.clone()
        };
        auth_url
            .set_host(Some(&auth_host))
            .map_err(|_| Self::invalid_uri("Invalid SFTP host"))?;
        auth_url
            .set_port(Some(parsed.port))
            .map_err(|_| Self::invalid_uri("Invalid SFTP port"))?;
        if let Some(username) = parsed.username.as_deref() {
            auth_url
                .set_username(username)
                .map_err(|_| Self::invalid_uri("Invalid SFTP username"))?;
            if let Some(password) = parsed.password.as_deref() {
                auth_url
                    .set_password(Some(password))
                    .map_err(|_| Self::invalid_uri("Invalid SFTP password"))?;
            }
        }

        let mut factory = AuthConfigFactory::new();
        if !options.no_netrc {
            let netrc_path = options.netrc_path.clone().or_else(find_netrc_file);
            if let Some(netrc_path) = netrc_path
                && let Err(error) = factory.load_netrc_file(std::path::Path::new(&netrc_path))
            {
                tracing::debug!("Failed to load SFTP netrc file {netrc_path}: {error}");
            }
        }

        let auth_options = AuthResolveOptions {
            no_netrc: options.no_netrc,
            ftp_user: options.ftp_user.clone(),
            ftp_passwd: options.ftp_passwd.clone(),
            ..AuthResolveOptions::default()
        };
        let credentials = factory
            .resolve(&auth_url, parsed.password.is_some(), &auth_options)
            .ok_or_else(|| Self::invalid_uri("Unable to resolve SFTP credentials"))?;

        Ok((
            credentials.user().to_string(),
            Some(credentials.password().to_string()),
        ))
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
        let (connect_timeout, read_timeout, compression, keepalive_interval, preferred_ciphers) = {
            let group = self.group();
            let options = group.options();
            (
                options
                    .sftp_connect_timeout
                    .unwrap_or(constants::SFTP_CONNECT_TIMEOUT_SECS),
                options
                    .sftp_read_timeout
                    .unwrap_or(constants::SFTP_READ_TIMEOUT_SECS),
                options.sftp_compression,
                options
                    .sftp_keepalive_interval
                    .unwrap_or(aria2_protocol::DEFAULT_SFTP_KEEPALIVE_INTERVAL_SECS),
                options.sftp_preferred_ciphers.clone().unwrap_or_default(),
            )
        };
        let mut opts = SshOptions::new(&self.host, &self.username)
            .with_port(self.port)
            .with_timeouts(
                Duration::from_secs(connect_timeout),
                Duration::from_secs(read_timeout),
            )
            .with_compression(compression)
            .with_keepalive_interval(
                (keepalive_interval != 0).then(|| Duration::from_secs(keepalive_interval)),
            )
            .with_preferred_ciphers(preferred_ciphers)
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
        if err.is_retryable() {
            return Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: err.to_string(),
            });
        }

        match err {
            SshError::AuthFailed { .. } => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("{}:{}", host, port),
            }),
            SshError::NoCredentials { .. } => Aria2Error::Fatal(FatalError::Config(format!(
                "No SSH credentials provided for {}:{}",
                host, port
            ))),
            _ => Aria2Error::Fatal(FatalError::Config(format!(
                "SFTP connection to {}:{} failed: {}",
                host, port, err
            ))),
        }
    }

    /// Map a FileOpError to the appropriate Aria2Error for engine-level handling.
    pub(super) fn map_file_op_error(err: &FileOpError, host: &str, path: &str) -> Aria2Error {
        match err {
            FileOpError::NotFound { .. } => {
                Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
            }
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
            FileOpError::Session { source, .. } if source.is_network() => {
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
