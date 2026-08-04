//! Core types for FTP download command.
//!
//! Contains the `FtpDownloadCommand` struct definition, constructors,
//! URI parsing, filename extraction, and FTP error classification.

use std::{net::SocketAddr, sync::Arc};

use tracing::info;

use crate::dns::dns_cache::DnsCache;
use crate::network::ConnectionContext;

use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::control::urlencoding_decode;

/// FTP download command that handles the complete download lifecycle
pub struct FtpDownloadCommand {
    pub(super) group: Arc<std::sync::RwLock<RequestGroup>>,
    pub(super) output_path: std::path::PathBuf,
    pub(super) started: bool,
    pub(super) completed_bytes: u64,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) remote_path: String,
    pub(super) username: String,
    pub(super) password: String,
    /// Resume offset for partial downloads (0 if not resuming)
    pub(super) resume_offset: u64,
    /// Whether to use passive mode (true) or active mode (false)
    pub(super) passive_mode: bool,
    /// Maximum number of retry attempts for transient errors
    pub(super) max_retries: u32,
    /// Current retry attempt count
    pub(super) current_retry: u32,
    pub(super) last_connection_context: Option<ConnectionContext>,
    pub(super) resolved_addresses: Vec<SocketAddr>,
    pub(super) dns_cache: Option<Arc<tokio::sync::Mutex<DnsCache>>>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` for this download.
    pub(super) global_limiter: Option<RateLimiter>,
}

impl FtpDownloadCommand {
    /// Create a new FTP download command
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group(group, output_dir, output_name)
    }

    /// Create an FTP download command that reuses an externally-managed
    /// `RequestGroup` (e.g. from the engine's promotion flow).
    ///
    /// The first URI in the group is used to extract FTP parameters
    /// (host, port, credentials, remote path). Output directory and filename
    /// fall back to the group's `DownloadOptions` when not explicitly
    /// overridden. The group's existing GID and progress counters are reused.
    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let (uri, options) = {
            let g = group.recover();
            let uri = g.uris().first().cloned().ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config(
                    "RequestGroup has no URIs for FTP download".into(),
                ))
            })?;
            let opts = g.options_arc();
            (uri, opts)
        };

        let (host, port, username, password, remote_path) = Self::parse_uri(&uri)?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(&remote_path))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Check if file exists for resume support
        let resume_offset = if path.exists() {
            std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        info!(
            "FtpDownloadCommand created (shared group): {} -> {} ({}:{}/{}) [resume_offset={}]",
            uri,
            path.display(),
            host,
            port,
            remote_path,
            resume_offset
        );

        Ok(Self {
            group,
            output_path: path,
            started: false,
            completed_bytes: 0,
            host,
            port,
            remote_path,
            username,
            password,
            resume_offset,
            passive_mode: true, // Default to passive mode
            max_retries: constants::DEFAULT_MAX_RETRIES,
            current_retry: 0,
            last_connection_context: None,
            resolved_addresses: Vec::new(),
            dns_cache: None,
            global_limiter: None,
        })
    }

    /// Parse FTP URI into components
    pub fn set_resolved_addresses(&mut self, addresses: Vec<SocketAddr>) {
        self.resolved_addresses = addresses;
    }

    pub fn set_dns_cache(&mut self, dns_cache: Arc<tokio::sync::Mutex<DnsCache>>) {
        self.dns_cache = Some(dns_cache);
    }

    pub(super) fn parse_uri(uri: &str) -> Result<(String, u16, String, String, String)> {
        if !uri.starts_with("ftp://") && !uri.starts_with("ftps://") {
            return Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol {
                protocol: "ftp".into(),
            }));
        }

        let without_scheme = uri
            .trim_start_matches("ftp://")
            .trim_start_matches("ftps://");

        let (auth_host_port, path) = match without_scheme.find('/') {
            Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
            None => (without_scheme, "/"),
        };

        let (auth, host_port) = match auth_host_port.rfind('@') {
            Some(idx) => (&auth_host_port[..idx], &auth_host_port[idx + 1..]),
            None => ("", auth_host_port),
        };

        let (username, password) = if auth.is_empty() {
            (
                constants::FTP_DEFAULT_USER.to_string(),
                constants::FTP_DEFAULT_PASSWORD.to_string(),
            )
        } else if let Some(colon_pos) = auth.find(':') {
            (
                auth[..colon_pos].to_string(),
                auth[colon_pos + 1..].to_string(),
            )
        } else {
            (auth.to_string(), String::new())
        };

        let (host, port) = match host_port.rfind(':') {
            Some(idx) => (
                host_port[..idx].to_string(),
                host_port[idx + 1..]
                    .parse::<u16>()
                    .unwrap_or(constants::FTP_DEFAULT_PORT),
            ),
            None => (host_port.to_string(), constants::FTP_DEFAULT_PORT),
        };

        Ok((host, port, username, password, urlencoding_decode(path)))
    }

    /// Extract filename from remote path
    pub(super) fn extract_filename(remote_path: &str) -> Option<String> {
        remote_path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "/")
            .map(|s| s.to_string())
    }

    /// Get read access to the request group
    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, the disk writer created in `execute_single_attempt` acquires
    /// tokens from this limiter in addition to the per-download limiter.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
    }

    /// Classify FTP response code to determine error handling strategy
    #[allow(dead_code)] // Must remain: will be used when FTP retry-with-classification logic is integrated into execute_single_attempt
    pub(super) fn classify_ftp_error(&self, code: u16, message: &str) -> Aria2Error {
        match code {
            // Positive responses (should not be errors)
            100..=399 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Unexpected positive response: {} {}", code, message),
            }),
            // Transient negative completion - retry may succeed
            421 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Service not available: {}", message),
            }),
            425 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Can't open data connection: {}", message),
            }),
            426 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Connection closed; transfer aborted: {}", message),
            }),
            450 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Requested file action not taken: {}", message),
            }),
            451 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Requested action aborted: {}", message),
            }),
            452 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Requested action not taken: {}", message),
            }),
            // Permanent negative completion - do not retry
            500..=504 => Aria2Error::Fatal(FatalError::Config(format!(
                "FTP syntax error: {} {}",
                code, message
            ))),
            530 => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("{}:{}", self.host, self.port),
            }),
            532 => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: "Account required for storing file".into(),
            }),
            550 => Aria2Error::Fatal(FatalError::FileNotFound {
                path: self.remote_path.clone(),
            }),
            551 => Aria2Error::Fatal(FatalError::Config(format!(
                "Page type unknown: {}",
                message
            ))),
            552 => Aria2Error::Fatal(FatalError::Config(format!(
                "Exceeded storage allocation: {}",
                message
            ))),
            553 => Aria2Error::Fatal(FatalError::PermissionDenied {
                path: format!("Filename not allowed: {}", message),
            }),
            // Unknown error codes
            _ => {
                // Check message content for hints about error type
                let msg_lower = message.to_lowercase();
                if msg_lower.contains("not found")
                    || msg_lower.contains("no such")
                    || msg_lower.contains("access denied")
                    || msg_lower.contains("permission")
                {
                    Aria2Error::Fatal(FatalError::FileNotFound {
                        path: self.remote_path.clone(),
                    })
                } else if msg_lower.contains("login") || msg_lower.contains("auth") {
                    Aria2Error::Fatal(FatalError::PermissionDenied {
                        path: format!("{}:{}", self.host, self.port),
                    })
                } else {
                    // Default to recoverable for unknown codes in 4xx/5xx range
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("FTP error {} {}: {}", code, message, self.remote_path),
                    })
                }
            }
        }
    }
}
