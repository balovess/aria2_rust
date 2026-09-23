mod execute;
mod tail_reclaim;
#[cfg(test)]
mod tests;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::constants;
use crate::engine::command::{PROGRESS_CHANNEL_CAPACITY, ProgressUpdate};
use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::error::{Aria2Error, Result};
use crate::http::HttpRequestPolicy;
use crate::http::cookie::Cookie;
use crate::http::cookie::CookieStorage;
use crate::http::socks_connector::{NoProxyMatcher, ProxyUrl};
use crate::network::OutboundNetworkPolicy;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::perf_monitor::{AtomicMetrics, Metrics, PerformanceMonitor};
use crate::util::rwlock_ext::RwLockRecover;
use crate::validation::uri::sanitize_filename_from_uri;

/// Core download command that handles HTTP/HTTPS file downloads.
///
/// Supports both sequential and concurrent (range-based) download strategies,
/// with automatic resume, cookie management, proxy configuration, and
/// checksum verification.
pub struct DownloadCommand {
    pub(super) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters -- avoids RwLock on the hot path.
    pub(super) progress: Arc<AtomicProgress>,
    pub(super) client: Arc<reqwest::Client>,
    pub(super) outbound_network_policy: Arc<OutboundNetworkPolicy>,
    pub(super) output_path: std::path::PathBuf,
    /// Whether the filename came from an explicit `--out`/metadata name.
    /// Implicit HTTP names may be replaced by response metadata before I/O.
    pub(super) output_name_explicit: bool,
    /// Whether the output path has already gone through collision resolution.
    /// Mirror failover reuses this resolved path after the prior attempt
    /// releases its temporary registry claim.
    pub(super) output_path_resolved: bool,
    pub(super) started: bool,
    pub(super) completed: bool,
    pub(super) completed_bytes: u64,
    pub(super) file_allocation: String,
    pub(super) mmap_threshold: u64,
    pub(super) secure_falloc: bool,
    /// `--check-integrity`: verify existing data against context piece hashes
    /// before downloading (C++ `CheckIntegrityMan`). Only meaningful when the
    /// DownloadContext carries piece hashes (e.g. Metalink).
    pub(super) check_integrity: bool,
    pub(super) cookie_storage: Arc<CookieStorage>,
    pub(super) cookie_file: Option<String>,
    pub(super) no_proxy_matcher: Option<NoProxyMatcher>,
    pub(super) stat_man: Arc<ServerStatMan>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` / segment download loops
    /// so that all concurrent downloads share a single bandwidth ceiling.
    pub(super) global_limiter: Option<RateLimiter>,
    pub(super) perf_monitor: Option<Arc<PerformanceMonitor>>,
    pub(super) atomic_metrics: Arc<AtomicMetrics>,
    pub(super) request_policy: HttpRequestPolicy,
    pub(super) progress_sender: Option<mpsc::Sender<ProgressUpdate>>,
    pub(super) progress_receiver: Option<mpsc::Receiver<ProgressUpdate>>,
    pub(super) progress_aggregator_handle: Option<tokio::task::JoinHandle<()>>,

    // ── Tail reclaim progress tracking ─────────────────────────────────
    // Mirrors C++ DownloadCommand fields:
    //   lastTailReclaimSessionDownloadLength_, tailReclaimLastProgress_,
    //   startupIdleTime_, lowestDownloadSpeedLimit_
    //
    // These fields track when data was last received so that the tail
    // reclaim policy can detect stalled connections.  In C++ these are
    // updated on every data chunk via updateTailReclaimProgress().  In Rust
    // they are updated via update_tail_reclaim_progress() which reads from
    // the lock-free AtomicProgress counter.
    /// Completed length at the last time progress was detected.
    /// Mirrors C++ `lastTailReclaimSessionDownloadLength_`.
    pub(super) last_tail_reclaim_session_download_length: u64,

    /// Timestamp of the last time progress was detected.
    /// Mirrors C++ `tailReclaimLastProgress_`.
    pub(super) tail_reclaim_last_progress: Instant,

    /// Stall threshold — if no progress for this duration, the connection
    /// is considered stalled.  Mirrors C++ `startupIdleTime_`.
    /// Defaults to 10 seconds (C++ `PREF_STARTUP_IDLE_TIME` default).
    pub(super) startup_idle_time: Duration,

    /// Lowest download speed limit in bytes/sec.  Downloads slower than
    /// this are aborted.  Mirrors C++ `lowestDownloadSpeedLimit_`.
    /// 0 means no limit.
    pub(super) lowest_speed_limit: u64,
}

fn uri_host(uri: &str) -> Option<String> {
    reqwest::Url::parse(uri).ok()?.host_str().map(str::to_owned)
}

/// Return the actual HTTP proxy endpoint used for an HTTP(S) URI.
///
/// The endpoint, rather than the origin, determines the address family of the
/// first outbound socket.  This helper is shared with the async command
/// factory so proxy DNS is resolved before the synchronous reqwest client is
/// built.
pub(crate) fn http_proxy_origin(uri: &str, options: &DownloadOptions) -> Option<(String, u16)> {
    let parsed_uri = url::Url::parse(uri).ok()?;
    let hostname = parsed_uri.host_str()?;
    let target_port = parsed_uri.port_or_known_default()?;

    if let Some(no_proxy) = options.no_proxy.as_deref() {
        let matcher = NoProxyMatcher::from_env_value(no_proxy);
        let bypassed = hostname
            .parse::<IpAddr>()
            .map(|address| matcher.should_bypass(&std::net::SocketAddr::new(address, target_port)))
            .unwrap_or_else(|_| matcher.should_bypass_hostname(hostname));
        if bypassed {
            return None;
        }
    }

    let candidate = match parsed_uri.scheme() {
        "http" => options
            .http_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
            .or_else(|| {
                options
                    .all_proxy
                    .as_deref()
                    .filter(|proxy| !proxy.is_empty())
            }),
        "https" => options
            .https_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
            .or_else(|| {
                options
                    .all_proxy
                    .as_deref()
                    .filter(|proxy| !proxy.is_empty())
            }),
        _ => None,
    }?;
    let proxy = ProxyUrl::parse(candidate).ok()?;
    if !matches!(
        proxy.protocol,
        crate::http::socks_connector::ProxyProtocol::Http
            | crate::http::socks_connector::ProxyProtocol::Https
    ) {
        return None;
    }
    Some((proxy.host, proxy.port))
}

fn source_for_remotes(
    policy: &OutboundNetworkPolicy,
    remotes: &[std::net::SocketAddr],
) -> std::io::Result<Option<IpAddr>> {
    let mut last_error = None;
    for remote in remotes {
        match policy.source_for(*remote) {
            Ok(source) => return Ok(source),
            Err(error) => last_error = Some(error),
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    Ok(policy.addresses().into_iter().next())
}

pub(crate) struct ResolvedNetworkAddresses {
    pub(crate) target: Option<Vec<std::net::SocketAddr>>,
    pub(crate) proxy: Option<Vec<std::net::SocketAddr>>,
}

fn apply_local_address(
    builder: reqwest::ClientBuilder,
    local_address: Option<IpAddr>,
) -> reqwest::ClientBuilder {
    match local_address {
        Some(address) => builder.local_address(address),
        None => builder,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProxyTarget {
    Http,
    Https,
    All,
}

pub(crate) fn build_reqwest_proxy(
    target: ProxyTarget,
    proxy_url: &str,
    username: Option<&str>,
    password: Option<&str>,
    no_proxy: Option<&str>,
) -> std::result::Result<reqwest::Proxy, reqwest::Error> {
    let mut proxy = match target {
        ProxyTarget::Http => reqwest::Proxy::http(proxy_url)?,
        ProxyTarget::Https => reqwest::Proxy::https(proxy_url)?,
        ProxyTarget::All => reqwest::Proxy::all(proxy_url)?,
    };

    // Preserve credentials embedded in the proxy URL unless an option
    // explicitly overrides them, matching AbstractCommand::makeProxyUri().
    if username.is_some() || password.is_some() {
        let embedded = proxy_url.parse::<reqwest::Url>().ok();
        let embedded_user = embedded
            .as_ref()
            .filter(|url| !url.username().is_empty())
            .map(|url| url.username().to_string());
        let embedded_password = embedded
            .as_ref()
            .and_then(|url| url.password().map(str::to_string));
        let effective_user = username.map(str::to_owned).or(embedded_user);
        let effective_password = password
            .map(str::to_owned)
            .or(embedded_password)
            .unwrap_or_default();

        if let Some(user) = effective_user {
            proxy = proxy.basic_auth(&user, &effective_password);
        }
    }

    if let Some(no_proxy) = no_proxy {
        proxy = proxy.no_proxy(reqwest::NoProxy::from_string(no_proxy));
    }

    Ok(proxy)
}

pub(crate) fn add_reqwest_proxy(
    builder: reqwest::ClientBuilder,
    target: ProxyTarget,
    proxy_url: &str,
    credentials: (Option<String>, Option<String>),
    no_proxy: Option<&str>,
) -> reqwest::ClientBuilder {
    match build_reqwest_proxy(
        target,
        proxy_url,
        credentials.0.as_deref(),
        credentials.1.as_deref(),
        no_proxy,
    ) {
        Ok(proxy) => builder.proxy(proxy),
        Err(error) => {
            warn!(%proxy_url, ?target, %error, "Ignoring invalid HTTP proxy configuration");
            builder
        }
    }
}

impl DownloadCommand {
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
        Self::new_with_group(group, uri, options, output_dir, output_name)
    }

    /// Create the unified HTTP command from a single-file Metalink document.
    #[cfg(feature = "metalink")]
    pub fn new_from_metalink(
        gid: GroupId,
        metalink_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let document =
            aria2_protocol::metalink::parser::MetalinkDocument::parse(metalink_bytes, None)
                .map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Metalink parse failed: {error}"
                    )))
                })?;
        let file = document.files.first().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Metalink contains no files".into(),
            ))
        })?;
        let urls: Vec<String> = file
            .get_sorted_urls()
            .iter()
            .map(|entry| entry.url.clone())
            .collect();
        let first_url = urls.first().cloned().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "No URLs in Metalink".into(),
            ))
        })?;

        let mut effective_options = options.clone();
        if effective_options.checksum.is_none()
            && let Some(hash) = file.hashes.first()
        {
            effective_options.checksum =
                Some((hash.algo.as_standard_name().to_string(), hash.value.clone()));
        }

        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            urls,
            effective_options.clone(),
        )));
        if let Some(size) = file.size {
            group.recover().set_total_length(size);
        }
        group.recover().set_output_name(file.name.clone());

        Self::new_with_group(
            group,
            &first_url,
            &effective_options,
            output_dir,
            Some(&file.name),
        )
    }

    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        Self::new_with_group_and_resolved_addresses(
            group,
            uri,
            options,
            output_dir,
            output_name,
            None,
        )
    }

    pub fn new_with_group_and_resolved_addresses(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        resolved_addresses: Option<Vec<std::net::SocketAddr>>,
    ) -> Result<Self> {
        Self::new_with_group_and_resolved_network_addresses(
            group,
            uri,
            options,
            output_dir,
            output_name,
            ResolvedNetworkAddresses {
                target: resolved_addresses,
                proxy: None,
            },
            Arc::new(OutboundNetworkPolicy::direct()),
        )
    }

    pub fn new_with_group_and_resolved_addresses_and_policy(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        resolved_addresses: Option<Vec<std::net::SocketAddr>>,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<Self> {
        Self::new_with_group_and_resolved_network_addresses(
            group,
            uri,
            options,
            output_dir,
            output_name,
            ResolvedNetworkAddresses {
                target: resolved_addresses,
                proxy: None,
            },
            outbound_network_policy,
        )
    }

    pub(crate) fn new_with_group_and_resolved_network_addresses(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        resolved_addresses: ResolvedNetworkAddresses,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Result<Self> {
        if options.uses_memory_download_for_uri(uri) {
            group.recover().mark_in_memory_download();
        }
        let progress = group
            .try_read()
            .map(|g| g.progress.clone())
            .unwrap_or_else(|_| Arc::new(AtomicProgress::new()));
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .unwrap_or_else(|| sanitize_filename_from_uri(uri));

        let path = std::path::PathBuf::from(&dir).join(&filename);
        group
            .recover()
            .set_resolved_output_path(path.to_string_lossy());
        let request_policy = options.http_request_policy();

        // Every client construction path below must use the same rustls
        // provider. The DNS-cache and proxy branches build custom clients
        // instead of reusing the global pool client.
        crate::http::client_pool::ensure_rustls_provider();
        let client_tls =
            crate::http::client_identity::ClientTlsConfig::from_download_options(options);
        let no_proxy = ![
            options.http_proxy.as_deref(),
            options.https_proxy.as_deref(),
            options.all_proxy.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|proxy| !proxy.is_empty());
        let has_custom_tls = client_tls.requires_custom_client();
        let proxy_origin = http_proxy_origin(uri, options);
        let target_remote = resolved_addresses
            .target
            .as_ref()
            .and_then(|addresses| addresses.first().copied())
            .or_else(|| {
                uri_host(uri).and_then(|host| {
                    host.parse::<std::net::IpAddr>().ok().map(|ip| {
                        std::net::SocketAddr::new(
                            ip,
                            url::Url::parse(uri)
                                .ok()
                                .and_then(|url| url.port_or_known_default())
                                .unwrap_or(80),
                        )
                    })
                })
            });
        let local_address = if proxy_origin.is_some() {
            let literal_proxy_remote = proxy_origin.as_ref().and_then(|(host, port)| {
                host.parse::<IpAddr>()
                    .ok()
                    .map(|address| std::net::SocketAddr::new(address, *port))
            });
            let proxy_remotes = resolved_addresses
                .proxy
                .as_deref()
                .filter(|addresses| !addresses.is_empty())
                .map(|addresses| addresses.to_vec())
                .or_else(|| literal_proxy_remote.map(|remote| vec![remote]));
            source_for_remotes(
                &outbound_network_policy,
                proxy_remotes.as_deref().unwrap_or_default(),
            )
            .map_err(|error| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Outbound network policy cannot serve HTTP proxy for {uri}: {error}"
                )))
            })?
        } else {
            target_remote
                .map(|remote| outbound_network_policy.source_for(remote))
                .transpose()
                .map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Outbound network policy cannot serve {uri}: {error}"
                    )))
                })?
                .flatten()
                .or_else(|| outbound_network_policy.addresses().into_iter().next())
        };
        let client = if no_proxy {
            if let Some(addresses) = resolved_addresses.target.as_deref()
                && !addresses.is_empty()
            {
                let host = uri_host(uri).ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "Unable to extract HTTP hostname for DNS cache override".to_string(),
                    ))
                })?;
                let builder = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(
                        constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
                    ))
                    .gzip(options.http_accept_gzip)
                    .user_agent(constants::USER_AGENT)
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve_to_addrs(&host, addresses);
                let builder = apply_local_address(builder, local_address);
                let builder = crate::http::client_identity::apply(builder, &client_tls)?;
                Arc::new(builder.build().map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Failed to build HTTP client with DNS cache: {e}"
                    )))
                })?)
            } else if has_custom_tls {
                let builder = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(
                        constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
                    ))
                    .gzip(options.http_accept_gzip)
                    .user_agent(constants::USER_AGENT)
                    .redirect(reqwest::redirect::Policy::none())
                    .pool_max_idle_per_host(constants::HTTP_CLIENT_POOL_MAX_IDLE_PER_HOST)
                    .pool_idle_timeout(Some(Duration::from_secs(
                        constants::HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
                    )))
                    .tcp_keepalive(Some(Duration::from_secs(
                        constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
                    )));
                let builder = apply_local_address(builder, local_address);
                let builder = crate::http::client_identity::apply(builder, &client_tls)?;
                Arc::new(builder.build().map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Failed to build HTTP client: {error}"
                    )))
                })?)
            } else {
                crate::http::client_pool::get_bound_client(local_address, options.http_accept_gzip)
            }
        } else {
            let mut builder = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
                ))
                .gzip(options.http_accept_gzip)
                .user_agent(constants::USER_AGENT)
                // Redirects are handled by SequentialDownloader so direct,
                // DNS-pinned, and proxied clients share one URI/retry seam.
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(constants::HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST)
                .pool_idle_timeout(Some(std::time::Duration::from_secs(
                    constants::HTTP_DEFAULT_POOL_IDLE_TIMEOUT_SECS,
                )))
                .tcp_keepalive(Some(std::time::Duration::from_secs(
                    constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
                )));

            let no_proxy = options.no_proxy.as_deref();

            if let Some(proxy) = options
                .http_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
            {
                builder = add_reqwest_proxy(
                    builder,
                    ProxyTarget::Http,
                    proxy,
                    options.proxy_credentials_for_scheme("http"),
                    no_proxy,
                );
            }

            if let Some(proxy) = options
                .https_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
            {
                builder = add_reqwest_proxy(
                    builder,
                    ProxyTarget::Https,
                    proxy,
                    options.proxy_credentials_for_scheme("https"),
                    no_proxy,
                );
            }

            if let Some(all_proxy) = options
                .all_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
            {
                match ProxyUrl::parse(all_proxy) {
                    Ok(parsed) => match parsed.protocol {
                        crate::http::socks_connector::ProxyProtocol::Http
                        | crate::http::socks_connector::ProxyProtocol::Https => {
                            builder = add_reqwest_proxy(
                                builder,
                                ProxyTarget::All,
                                all_proxy,
                                options.proxy_credentials_for_scheme("all"),
                                no_proxy,
                            );
                        }
                        _ => {
                            tracing::info!(
                                "SOCKS proxy configured ({}) - use SocksConnector for direct TCP connections",
                                all_proxy
                            );
                        }
                    },
                    Err(e) => {
                        warn!("Failed to parse all-proxy URL '{}': {}", all_proxy, e);
                    }
                }
            }

            let builder = apply_local_address(builder, local_address);
            let builder = crate::http::client_identity::apply(builder, &client_tls)?;
            let client = builder.build().map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Failed to build HTTP client: {}",
                    e
                )))
            })?;

            Arc::new(client)
        };

        info!("DownloadCommand created: {} -> {}", uri, path.display());

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = CookieStorage::shared();

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::channel::<ProgressUpdate>(PROGRESS_CHANNEL_CAPACITY);

        Ok(Self {
            group,
            progress,
            client,
            outbound_network_policy,
            output_path: path,
            output_name_explicit: output_name.is_some(),
            output_path_resolved: false,
            started: false,
            completed: false,
            completed_bytes: 0,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| constants::DEFAULT_FILE_ALLOCATION.to_string()),
            mmap_threshold: options.mmap_threshold.unwrap_or(256 * 1024 * 1024),
            secure_falloc: options.secure_falloc,
            check_integrity: options.check_integrity,
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: ServerStatMan::shared().clone(),
            global_limiter: None,
            perf_monitor: None,
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            request_policy,
            progress_sender: Some(progress_tx),
            progress_receiver: Some(progress_rx),
            progress_aggregator_handle: None,
            // Tail reclaim fields — mirrors C++ DownloadCommand constructor.
            last_tail_reclaim_session_download_length: 0,
            tail_reclaim_last_progress: Instant::now(),
            startup_idle_time: Duration::from_secs(options.startup_idle_time.unwrap_or(10)),
            lowest_speed_limit: options.lowest_speed_limit.unwrap_or(0),
        })
    }

    fn load_cookies(
        cookie_storage: &Arc<CookieStorage>,
        cookie_file: &Option<String>,
        uri: &str,
        options: &DownloadOptions,
    ) {
        if let Some(cf) = cookie_file {
            let p = std::path::Path::new(cf);
            if p.exists() {
                match cookie_storage.load_file(p) {
                    Ok(n) => info!("Loaded {} cookies from file: {}", n, cf),
                    Err(e) => warn!("Failed to load cookie file {}: {}", cf, e),
                }
            }
        }

        if let Some(ref cookies_str) = options.cookies {
            let domain = Self::extract_host(uri);
            for pair in cookies_str.split(';') {
                let pair = pair.trim();
                if pair.is_empty() {
                    continue;
                }
                if let Some((name, value)) = pair.split_once('=') {
                    let name = name.trim();
                    let value = value.trim();
                    if !name.is_empty() {
                        cookie_storage.add(Cookie::new(name, value, &domain));
                    }
                }
            }
            if !cookie_storage.is_empty() {
                info!("Manually set {} cookies", cookie_storage.count());
            }
        }
    }

    pub fn new_with_client(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        client: Arc<reqwest::Client>,
    ) -> Result<Self> {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group_and_client(group, uri, options, output_dir, output_name, client)
    }

    pub fn new_with_group_and_client(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        client: Arc<reqwest::Client>,
    ) -> Result<Self> {
        let progress = group
            .try_read()
            .map(|g| g.progress.clone())
            .unwrap_or_else(|_| Arc::new(AtomicProgress::new()));
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .unwrap_or_else(|| sanitize_filename_from_uri(uri));

        let path = std::path::PathBuf::from(&dir).join(&filename);
        group
            .recover()
            .set_resolved_output_path(path.to_string_lossy());

        let request_policy = options.http_request_policy();
        info!(
            "DownloadCommand created (shared client): {} -> {}",
            uri,
            path.display()
        );

        let cookie_file = options.cookie_file.clone();
        let cookie_storage = CookieStorage::shared();

        Self::load_cookies(&cookie_storage, &cookie_file, uri, options);

        let (progress_tx, progress_rx) = mpsc::channel::<ProgressUpdate>(PROGRESS_CHANNEL_CAPACITY);

        Ok(Self {
            group,
            progress,
            client,
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            output_path: path,
            output_name_explicit: output_name.is_some(),
            output_path_resolved: false,
            started: false,
            completed: false,
            completed_bytes: 0,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| constants::DEFAULT_FILE_ALLOCATION.to_string()),
            mmap_threshold: options.mmap_threshold.unwrap_or(256 * 1024 * 1024),
            secure_falloc: options.secure_falloc,
            check_integrity: options.check_integrity,
            cookie_storage,
            cookie_file,
            no_proxy_matcher: options
                .no_proxy
                .as_ref()
                .map(|np| NoProxyMatcher::from_env_value(np)),
            stat_man: ServerStatMan::shared().clone(),
            global_limiter: None,
            perf_monitor: None,
            atomic_metrics: Arc::new(AtomicMetrics::new()),
            request_policy,
            progress_sender: Some(progress_tx),
            progress_receiver: Some(progress_rx),
            progress_aggregator_handle: None,
            // Tail reclaim fields — mirrors C++ DownloadCommand constructor.
            last_tail_reclaim_session_download_length: 0,
            tail_reclaim_last_progress: Instant::now(),
            startup_idle_time: Duration::from_secs(options.startup_idle_time.unwrap_or(10)),
            lowest_speed_limit: options.lowest_speed_limit.unwrap_or(0),
        })
    }

    pub fn new_with_stat_man(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
        stat_man: Arc<ServerStatMan>,
    ) -> Result<Self> {
        let mut cmd = Self::new(gid, uri, options, output_dir, output_name)?;
        cmd.stat_man = stat_man;
        Ok(cmd)
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, the download paths (sequential and concurrent) will acquire
    /// tokens from this limiter in addition to the per-download limiter,
    /// enforcing a global bandwidth ceiling across all concurrent downloads.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
    }

    pub fn enable_perf_monitor(&mut self) {
        self.perf_monitor = Some(Arc::new(PerformanceMonitor::new()));
    }

    #[allow(dead_code)]
    pub(crate) fn with_progress_sender(mut self, sender: mpsc::Sender<ProgressUpdate>) -> Self {
        self.progress_sender = Some(sender);
        self.progress_receiver = None;
        self
    }

    pub(crate) fn spawn_progress_aggregator(&mut self) {
        if self.progress_aggregator_handle.is_some() {
            return;
        }
        if let Some(rx) = self.progress_receiver.take() {
            let handle = crate::engine::download_engine::DownloadEngine::spawn_progress_aggregator(
                Arc::clone(&self.group),
                Arc::clone(&self.progress),
                rx,
            );
            self.progress_aggregator_handle = Some(handle);
        }
    }

    pub(crate) async fn drain_progress_aggregator(&mut self) {
        self.progress_sender = None;
        if let Some(handle) = self.progress_aggregator_handle.take()
            && let Err(e) = handle.await
        {
            warn!("Progress aggregator task ended unexpectedly: {}", e);
        }
    }

    pub fn get_perf_metrics(&self) -> Metrics {
        self.atomic_metrics.snapshot()
    }

    pub fn get_perf_report(&self) -> Option<String> {
        self.perf_monitor.as_ref().map(|m| m.export_text())
    }

    pub fn get_perf_report_json(&self) -> Option<String> {
        self.perf_monitor.as_ref().map(|m| m.export_json())
    }

    fn extract_host(uri: &str) -> String {
        reqwest::Url::parse(uri)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_else(|| constants::DEFAULT_HOST.to_string())
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    pub fn group_mut(&self) -> std::sync::RwLockWriteGuard<'_, RequestGroup> {
        self.group.recover_mut()
    }

    pub fn no_proxy_matcher(&self) -> Option<&NoProxyMatcher> {
        self.no_proxy_matcher.as_ref()
    }

    pub(super) fn should_use_concurrent(
        &self,
        total_length: u64,
        supports_range: bool,
        split: u16,
    ) -> bool {
        if self.group.recover().options().force_sequential {
            return false;
        }
        if !supports_range {
            return false;
        }
        if total_length < constants::CONCURRENT_MIN_FILE_SIZE as u64 {
            return false;
        }
        split > 1
    }

    pub(super) fn create_cookie_helper(&self) -> CookieHelper {
        CookieHelper::new(Arc::clone(&self.cookie_storage), self.cookie_file.clone())
    }

    pub(super) fn create_progress_updater(&self) -> ProgressUpdater {
        ProgressUpdater::new(
            self.progress_sender.clone(),
            self.group.recover().global_net_stat(),
            Arc::clone(&self.progress),
            Arc::clone(&self.atomic_metrics),
            self.perf_monitor.clone(),
        )
    }

    /// Non-blocking check whether the underlying RequestGroup has been
    /// cancelled (status set to Removed by aria2.remove /
    /// aria2.forceRemove) or paused (status set to Paused by
    /// aria2.pause / aria2.forcePause).
    ///
    /// Returns Err with a DownloadFailed error when the group has been
    /// removed or paused, so the caller can abort the download promptly.
    /// Uses try_read on the outer group lock so it is safe to call from
    /// hot download loops; when the lock is contended the method treats the
    /// download as still running (returns Ok(())) and the caller will
    /// re-check on the next iteration.
    pub(super) fn check_cancelled(&self) -> Result<()> {
        match self.group.try_read() {
            Ok(g) if g.is_removed() => Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            )),
            Ok(g) if g.is_paused_flag() => {
                Err(Aria2Error::DownloadFailed("Download paused".into()))
            }
            Ok(g) if g.is_force_halt_requested() => {
                Err(Aria2Error::DownloadFailed("Download halted".into()))
            }
            Ok(g) if g.is_halt_requested() => {
                Err(Aria2Error::DownloadFailed("Download halted".into()))
            }
            _ => Ok(()),
        }
    }
}
