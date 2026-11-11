//! Core types for FTP download command.
//!
//! Contains the `FtpDownloadCommand` struct definition, constructors,
//! URI parsing, filename extraction, and request configuration.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use tracing::info;
use url::Url;

use crate::dns::dns_cache::DnsCache;
use crate::engine::progress_checkpoint::ProgressCheckpoint;
use crate::engine::retry_policy::RetryPolicy;
use aria2_protocol::ftp::tls::{FtpsConfig, TlsVersion};

use crate::ftp::connection::{FtpProxyConfig, ProxyMethod};
use crate::http::socks_connector::NoProxyMatcher;
use crate::network::ConnectionContext;
use crate::network::OutboundNetworkPolicy;

use crate::constants;
use crate::error::{Aria2Error, FatalError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

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
    /// TLS configuration for an `ftps://` URI. Plain FTP keeps this unset.
    pub(super) ftps_config: Option<FtpsConfig>,
    /// Use implicit TLS for the conventional FTPS port 990.
    pub(super) ftps_implicit: bool,
    /// Resume offset for partial downloads (0 if not resuming)
    pub(super) resume_offset: u64,
    /// Whether to use passive mode (true) or active mode (false)
    pub(super) passive_mode: bool,
    /// Timeout for establishing and greeting the FTP control connection.
    pub(super) connect_timeout: Duration,
    /// Unified retry contract for the download. `max_retries` in the policy
    /// means total attempts and `0` means unlimited.
    pub(super) retry_policy: RetryPolicy,
    pub(super) last_connection_context: Option<ConnectionContext>,
    pub(super) resolved_addresses: Vec<SocketAddr>,
    pub(super) outbound_network_policy: Arc<OutboundNetworkPolicy>,
    pub(super) dns_cache: Option<Arc<tokio::sync::Mutex<DnsCache>>>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` for this download.
    pub(super) global_limiter: Option<RateLimiter>,
    /// File allocation strategy selected by the request options.
    pub(super) file_allocation: String,
    pub(super) secure_falloc: bool,
    pub(super) checkpoint: Option<ProgressCheckpoint>,
}

pub(super) struct FtpDownloadTarget {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) username: String,
    pub(super) password: String,
    pub(super) remote_path: String,
    pub(super) secure: bool,
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
        if options.uses_memory_download_for_uri(uri) {
            group.recover().mark_in_memory_download();
        }
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

        if options.uses_memory_download_for_uri(&uri) {
            group.recover().mark_in_memory_download();
        }

        let target = Self::parse_uri(&uri)?;
        let ftps_implicit = target.secure && target.port == 990;
        let ftps_config = target.secure.then(|| Self::ftps_config(&options));

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(&target.remote_path))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Check if file exists for resume support
        let resume_offset = if group.recover().is_in_memory_download() {
            0
        } else if path.exists() {
            std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        info!(
            "FtpDownloadCommand created (shared group): {} -> {} ({}:{}/{}) [resume_offset={}]",
            uri,
            path.display(),
            target.host,
            target.port,
            target.remote_path,
            resume_offset
        );

        Ok(Self {
            group,
            output_path: path,
            started: false,
            completed_bytes: 0,
            host: target.host,
            port: target.port,
            remote_path: target.remote_path,
            username: target.username,
            password: target.password,
            ftps_config,
            ftps_implicit,
            resume_offset,
            passive_mode: options.ftp_pasv,
            connect_timeout: Duration::from_secs(
                options
                    .connect_timeout
                    .unwrap_or(constants::FTP_DEFAULT_CONNECT_TIMEOUT_SECS),
            ),
            retry_policy: RetryPolicy::new(
                options.max_retries,
                options.retry_wait.saturating_mul(1000),
            ),
            last_connection_context: None,
            resolved_addresses: Vec::new(),
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            dns_cache: None,
            global_limiter: None,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| crate::constants::DEFAULT_FILE_ALLOCATION.to_string()),
            secure_falloc: options.secure_falloc,
            checkpoint: None,
        })
    }

    /// Parse FTP URI into components
    pub fn set_resolved_addresses(&mut self, addresses: Vec<SocketAddr>) {
        self.resolved_addresses = addresses;
    }

    pub fn set_outbound_network_policy(&mut self, policy: Arc<OutboundNetworkPolicy>) {
        self.outbound_network_policy = policy;
    }

    pub fn set_dns_cache(&mut self, dns_cache: Arc<tokio::sync::Mutex<DnsCache>>) {
        self.dns_cache = Some(dns_cache);
    }

    /// Resolve the configured FTP proxy from typed task options.
    pub(super) fn ftp_proxy_config(&self) -> Result<Option<(FtpProxyConfig, ProxyMethod)>> {
        let options = {
            let group = self.group.recover();
            group.options_arc()
        };

        if let Some(no_proxy) = options.no_proxy.as_deref() {
            let matcher = NoProxyMatcher::from_env_value(no_proxy);
            let bypassed = self
                .host
                .parse::<IpAddr>()
                .ok()
                .map(|address| matcher.should_bypass(&SocketAddr::new(address, self.port)))
                .unwrap_or_else(|| matcher.should_bypass_hostname(&self.host));
            if bypassed {
                return Ok(None);
            }
        }

        let proxy_uri = options
            .ftp_proxy
            .as_deref()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                options
                    .all_proxy
                    .as_deref()
                    .filter(|value| !value.is_empty())
            });
        let Some(proxy_uri) = proxy_uri else {
            return Ok(None);
        };

        let proxy_url = Url::parse(proxy_uri).map_err(|error| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Invalid FTP proxy URL '{}': {}",
                proxy_uri, error
            )))
        })?;
        if !proxy_url.scheme().eq_ignore_ascii_case("http") {
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "Unsupported FTP proxy scheme '{}'; expected http",
                proxy_url.scheme()
            ))));
        }
        let proxy_host = proxy_url.host_str().ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config("FTP proxy URL has no host".to_string()))
        })?;
        let (username, password) = options.proxy_credentials_for_scheme("ftp");
        let embedded_username =
            (!proxy_url.username().is_empty()).then(|| proxy_url.username().to_string());
        let embedded_password = proxy_url.password().map(str::to_string);
        let proxy_username = username.or(embedded_username).unwrap_or_default();
        let proxy_password = password.or(embedded_password).unwrap_or_default();

        let explicit_method = ProxyMethod::parse(&options.proxy_method);
        let method = explicit_method.unwrap_or_default();

        Ok(Some((
            FtpProxyConfig {
                proxy_host: proxy_host.to_string(),
                proxy_port: proxy_url.port().unwrap_or(8080),
                proxy_username,
                proxy_password,
                ftp_username: self.username.clone(),
                connect_timeout: self.connect_timeout,
                user_agent: options
                    .user_agent
                    .clone()
                    .unwrap_or_else(|| constants::USER_AGENT.to_string()),
                explicit_proxy_method: Some(method),
            },
            method,
        )))
    }

    fn ftps_config(options: &DownloadOptions) -> FtpsConfig {
        let min_tls_version = match options
            .min_tls_version
            .as_deref()
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            Some("tlsv1.3") | Some("tls1.3") => TlsVersion::Tls13,
            _ => TlsVersion::Tls12,
        };

        FtpsConfig {
            enabled: true,
            check_certificate: options.check_certificate,
            ca_certificate: options
                .ca_certificate
                .as_deref()
                .map(std::path::PathBuf::from),
            min_tls_version,
        }
    }

    pub(super) fn parse_uri(uri: &str) -> Result<FtpDownloadTarget> {
        let parsed = aria2_protocol::ftp::uri::parse_uri(uri).ok_or_else(|| {
            Aria2Error::Fatal(FatalError::UnsupportedProtocol {
                protocol: "ftp".into(),
            })
        })?;
        let username = parsed
            .username
            .unwrap_or_else(|| constants::FTP_DEFAULT_USER.to_string());
        let password = parsed
            .password
            .unwrap_or_else(|| constants::FTP_DEFAULT_PASSWORD.to_string());
        Ok(FtpDownloadTarget {
            host: parsed.host,
            port: parsed.port,
            username,
            password,
            remote_path: parsed.remote_path,
            secure: parsed.secure,
        })
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
}
