//! Build the reqwest clients used by the HTTP download engine.
//!
//! This module owns HTTP-specific proxy selection, source-address binding,
//! TLS identity, and range-client pool configuration. The standalone
//! `aria2_protocol::http::HttpClient` remains an independent public client.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::constants;
use crate::error::{Aria2Error, Result};
use crate::http::socks_connector::{NoProxyMatcher, ProxyUrl};
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::DownloadOptions;

/// Resolved endpoints used to pin direct and proxied HTTP clients to the
/// already-selected DNS answers.
pub(crate) struct ResolvedNetworkAddresses {
    pub(crate) target: Option<Vec<std::net::SocketAddr>>,
    pub(crate) proxy: Option<Vec<std::net::SocketAddr>>,
}

pub(crate) fn range_client_pool_count(options: &DownloadOptions) -> usize {
    let max_connections = options
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        as usize;
    let requested_sessions = options
        .max_http2_sessions_per_server
        .unwrap_or(constants::DEFAULT_HTTP2_SESSIONS_PER_SERVER as u16)
        .clamp(1, constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        as usize;
    match options.http_version {
        crate::http::HttpVersion::Http2 => requested_sessions.min(max_connections),
        crate::http::HttpVersion::Auto | crate::http::HttpVersion::Http11 => max_connections,
    }
}

/// Build the default client and the independent clients used for concurrent
/// range requests, applying the outbound address policy to the actual first
/// hop (the origin or its HTTP proxy).
pub(super) fn build_download_clients(
    uri: &str,
    options: &DownloadOptions,
    client_tls: &crate::http::client_identity::ClientTlsConfig,
    resolved_addresses: &ResolvedNetworkAddresses,
    outbound_network_policy: &OutboundNetworkPolicy,
) -> Result<(Arc<reqwest::Client>, Arc<Vec<reqwest::Client>>)> {
    let proxy_disabled = ![
        options.http_proxy.as_deref(),
        options.https_proxy.as_deref(),
        options.all_proxy.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|proxy| !proxy.is_empty());
    let proxy_origin = http_proxy_origin(uri, options);
    let target_remote = resolved_addresses
        .target
        .as_ref()
        .and_then(|addresses| addresses.first().copied())
        .or_else(|| {
            uri_host(uri).and_then(|host| {
                host.parse::<IpAddr>().ok().map(|ip| {
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
            .map(<[_]>::to_vec)
            .or_else(|| literal_proxy_remote.map(|remote| vec![remote]));
        source_for_remotes(
            outbound_network_policy,
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

    let client = build_download_client(
        uri,
        options,
        client_tls,
        proxy_disabled,
        local_address,
        resolved_addresses,
    )?;
    let has_custom_tls = client_tls.requires_custom_client();
    let has_resolved_target = resolved_addresses
        .target
        .as_deref()
        .is_some_and(|addresses| !addresses.is_empty());
    let session_count = range_client_pool_count(options);
    let range_clients = if proxy_disabled && !has_resolved_target && !has_custom_tls {
        crate::http::client_pool::get_bound_download_clients_with_version(
            local_address,
            options.http_accept_gzip,
            session_count,
            options.http_version,
        )
    } else {
        let mut clients = Vec::with_capacity(session_count);
        clients.push(client.as_ref().clone());
        for _ in 1..session_count {
            clients.push(
                build_download_client(
                    uri,
                    options,
                    client_tls,
                    proxy_disabled,
                    local_address,
                    resolved_addresses,
                )?
                .as_ref()
                .clone(),
            );
        }
        Arc::new(clients)
    };

    Ok((client, range_clients))
}

/// Return the actual HTTP proxy endpoint used for an HTTP(S) URI.
///
/// The endpoint, rather than the origin, determines the address family of the
/// first outbound socket. The command factory uses this to resolve proxy DNS
/// before the synchronous reqwest client is built.
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

pub(super) fn uri_host(uri: &str) -> Option<String> {
    reqwest::Url::parse(uri).ok()?.host_str().map(str::to_owned)
}

pub(super) fn source_for_remotes(
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

fn apply_local_address(
    builder: reqwest::ClientBuilder,
    local_address: Option<IpAddr>,
) -> reqwest::ClientBuilder {
    match local_address {
        Some(address) => builder.local_address(address),
        None => builder,
    }
}

pub(super) fn build_download_client(
    uri: &str,
    options: &DownloadOptions,
    client_tls: &crate::http::client_identity::ClientTlsConfig,
    proxy_disabled: bool,
    local_address: Option<IpAddr>,
    resolved_addresses: &ResolvedNetworkAddresses,
) -> Result<Arc<reqwest::Client>> {
    let has_custom_tls = client_tls.requires_custom_client();
    if proxy_disabled {
        if let Some(addresses) = resolved_addresses
            .target
            .as_deref()
            .filter(|addresses| !addresses.is_empty())
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
            let builder = crate::http::client_identity::apply(builder, client_tls)?;
            return options
                .http_version
                .configure(crate::http::client_pool::configure_http2_download_client(
                    builder,
                ))
                .build()
                .map(Arc::new)
                .map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Failed to build HTTP client with DNS cache: {error}"
                    )))
                });
        }

        if has_custom_tls {
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
            let builder = crate::http::client_identity::apply(builder, client_tls)?;
            return options
                .http_version
                .configure(crate::http::client_pool::configure_http2_download_client(
                    builder,
                ))
                .build()
                .map(Arc::new)
                .map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Failed to build HTTP client: {error}"
                    )))
                });
        }

        return Ok(crate::http::client_pool::get_bound_client_with_version(
            local_address,
            options.http_accept_gzip,
            options.http_version,
        ));
    }

    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(
            constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
        ))
        .gzip(options.http_accept_gzip)
        .user_agent(constants::USER_AGENT)
        // Redirects are handled by SequentialDownloader so direct, DNS-pinned,
        // and proxied clients share one URI/retry seam.
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(constants::HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Some(Duration::from_secs(
            constants::HTTP_DEFAULT_POOL_IDLE_TIMEOUT_SECS,
        )))
        .tcp_keepalive(Some(Duration::from_secs(
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
            Err(error) => {
                warn!("Failed to parse all-proxy URL '{}': {}", all_proxy, error);
            }
        }
    }

    let builder = apply_local_address(builder, local_address);
    let builder = crate::http::client_identity::apply(builder, client_tls)?;
    options
        .http_version
        .configure(crate::http::client_pool::configure_http2_download_client(
            builder,
        ))
        .build()
        .map(Arc::new)
        .map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Failed to build HTTP client: {error}"
            )))
        })
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
