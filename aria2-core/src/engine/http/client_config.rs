//! Build the reqwest clients used by the HTTP download engine.
//!
//! This module owns HTTP-specific proxy selection, source-address binding,
//! TLS identity, and range-client pool configuration. The standalone
//! `aria2_protocol::http::HttpClient` remains an independent public client.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::constants;
use crate::error::{Aria2Error, Result};
use crate::http::socks_connector::NoProxyMatcher;
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::{DownloadOptions, HttpProxyOptions};
use aria2_protocol::proxy::{ProxyProtocol, ProxyUrl};

/// Resolved endpoints used to pin direct and proxied HTTP clients to the
/// already-selected DNS answers.
pub(crate) struct ResolvedNetworkAddresses {
    pub(crate) target: Option<Vec<std::net::SocketAddr>>,
    pub(crate) proxy: Option<Vec<std::net::SocketAddr>>,
    pub(crate) resolver: Option<Arc<dyn reqwest::dns::Resolve>>,
}

pub(crate) fn range_client_pool_count(options: &DownloadOptions) -> usize {
    let max_connections = options
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, constants::MAX_CONNECTION_PER_SERVER as u16) as usize;
    let requested_sessions = options
        .max_http2_sessions_per_server
        .unwrap_or(constants::DEFAULT_HTTP2_SESSIONS_PER_SERVER as u16)
        .clamp(1, constants::MAX_HTTP2_SESSIONS_PER_SERVER as u16)
        as usize;
    // A reqwest client already keeps multiple HTTP/1.1 connections alive, so
    // one client is only needed for each pool's worth of connections. HTTP/2
    // still needs independent clients to create independent TCP sessions.
    let connections_per_client = constants::HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST.max(1);
    let http1_clients = max_connections.div_ceil(connections_per_client).max(1);
    match options.http_version {
        crate::http::HttpVersion::Http2 => requested_sessions,
        crate::http::HttpVersion::Auto => requested_sessions.max(http1_clients),
        crate::http::HttpVersion::Http11 => http1_clients,
    }
}

/// Number of independent clients needed to give each usable DNS answer its
/// own preferred connection. Zero means the option is inactive.
pub(crate) fn multi_ip_client_count(options: &DownloadOptions, address_count: usize) -> usize {
    if !options.use_multiple_ips || address_count < 2 {
        return 0;
    }
    let max_connections = options
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, constants::MAX_CONNECTION_PER_SERVER as u16) as usize;
    let split = options.split.unwrap_or(constants::DEFAULT_SPLIT).max(1) as usize;
    let requested = match options.http_version {
        crate::http::HttpVersion::Http2 => range_client_pool_count(options),
        crate::http::HttpVersion::Http11 => split.min(max_connections),
        crate::http::HttpVersion::Auto => {
            range_client_pool_count(options).max(split.min(max_connections))
        }
    };
    address_count.min(requested.max(1))
}

/// Return a stable rotation whose first address is the one preferred by this
/// client. The original order is retained after the first address so failed
/// connections can still fall through to the remaining DNS answers.
pub(crate) fn rotated_addresses(addresses: &[SocketAddr], offset: usize) -> Vec<SocketAddr> {
    if addresses.is_empty() {
        return Vec::new();
    }
    let offset = offset % addresses.len();
    addresses[offset..]
        .iter()
        .chain(addresses[..offset].iter())
        .copied()
        .collect()
}

fn usable_target_addresses(
    addresses: &[SocketAddr],
    outbound_network_policy: &OutboundNetworkPolicy,
) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    addresses
        .iter()
        .copied()
        .filter(|address| {
            outbound_network_policy.is_direct()
                || outbound_network_policy.source_for(*address).is_ok()
        })
        .filter(|address| seen.insert(*address))
        .collect()
}

/// Select the DNS answers that should receive their own range client.
///
/// A mixed A/AAAA answer set is reduced to the resolver-preferred family when
/// that family already has enough addresses for multi-IP distribution. This
/// avoids adding a slower or unused address-family path merely because DNS
/// returned it. If the preferred family cannot provide two addresses, the
/// remaining answers are allowed to fill the address budget.
pub(crate) fn select_multi_ip_addresses(
    options: &DownloadOptions,
    addresses: &[SocketAddr],
    outbound_network_policy: &OutboundNetworkPolicy,
) -> Vec<SocketAddr> {
    let mut usable = usable_target_addresses(addresses, outbound_network_policy);
    let requested = multi_ip_client_count(options, usable.len());
    if requested < 2 {
        return Vec::new();
    }
    let Some(first) = usable.first() else {
        return Vec::new();
    };
    let preferred_is_ipv4 = first.is_ipv4();
    let preferred_count = usable
        .iter()
        .filter(|address| address.is_ipv4() == preferred_is_ipv4)
        .count();
    if preferred_count >= 2 {
        usable.retain(|address| address.is_ipv4() == preferred_is_ipv4);
    }
    usable.truncate(requested);
    usable
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
    let target_remotes = resolved_addresses
        .target
        .as_deref()
        .filter(|addresses| !addresses.is_empty())
        .map(<[_]>::to_vec)
        .or_else(|| {
            uri_host(uri).and_then(|host| {
                host.parse::<IpAddr>().ok().map(|ip| {
                    vec![std::net::SocketAddr::new(
                        ip,
                        url::Url::parse(uri)
                            .ok()
                            .and_then(|url| url.port_or_known_default())
                            .unwrap_or(80),
                    )]
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
        let source = source_for_remotes(
            outbound_network_policy,
            target_remotes.as_deref().unwrap_or_default(),
        )
        .map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Outbound network policy cannot serve {uri}: {error}"
            )))
        })?;
        source.or_else(|| outbound_network_policy.addresses().into_iter().next())
    };

    let client = build_download_client(
        uri,
        options,
        client_tls,
        proxy_disabled,
        local_address,
        resolved_addresses,
    )?;
    let connect_timeout = http_connect_timeout(options);
    let has_custom_tls = client_tls.requires_custom_client();
    let has_resolved_target = resolved_addresses
        .target
        .as_deref()
        .is_some_and(|addresses| !addresses.is_empty());
    let session_count = range_client_pool_count(options);
    let multi_ip_targets = if proxy_disabled && options.use_multiple_ips {
        resolved_addresses
            .target
            .as_deref()
            .map(|addresses| select_multi_ip_addresses(options, addresses, outbound_network_policy))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let multi_ip_session_count = multi_ip_client_count(options, multi_ip_targets.len());
    let range_clients = if multi_ip_session_count > 1 {
        info!(
            uri = %uri,
            addresses = multi_ip_session_count,
            "Using multiple resolved IPs for concurrent HTTP ranges"
        );
        let mut clients = Vec::with_capacity(multi_ip_session_count);
        for index in 0..multi_ip_session_count {
            let per_client_addresses = ResolvedNetworkAddresses {
                target: Some(rotated_addresses(&multi_ip_targets, index)),
                proxy: resolved_addresses.proxy.clone(),
                resolver: resolved_addresses.resolver.clone(),
            };
            clients.push(
                build_download_client(
                    uri,
                    options,
                    client_tls,
                    proxy_disabled,
                    local_address,
                    &per_client_addresses,
                )?
                .as_ref()
                .clone(),
            );
        }
        Arc::new(clients)
    } else if proxy_disabled && !has_resolved_target && !has_custom_tls {
        crate::http::client_pool::get_bound_download_clients_with_version_and_timeout(
            local_address,
            options.http_accept_gzip,
            session_count,
            options.http_version,
            connect_timeout,
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
    options.http_proxy_options().proxy_for_uri(uri)
}

/// Apply the task's HTTP proxy routes to a reqwest client.
///
/// Shared by ordinary HTTP downloads and BitTorrent WebSeeds so scheme
/// precedence, credentials, bypass rules, and invalid-proxy handling agree.
pub(crate) fn apply_proxy_options(
    mut builder: reqwest::ClientBuilder,
    options: &HttpProxyOptions,
) -> reqwest::ClientBuilder {
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
            options.credentials_for_scheme("http"),
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
            options.credentials_for_scheme("https"),
            no_proxy,
        );
    }
    if let Some(proxy) = options
        .all_proxy
        .as_deref()
        .filter(|proxy| !proxy.is_empty())
    {
        match ProxyUrl::parse(proxy) {
            Ok(parsed) => match parsed.protocol {
                ProxyProtocol::Http
                | ProxyProtocol::Https
                | ProxyProtocol::Socks4
                | ProxyProtocol::Socks5 => {
                    builder = add_reqwest_proxy(
                        builder,
                        ProxyTarget::All,
                        proxy,
                        options.credentials_for_scheme("all"),
                        no_proxy,
                    );
                }
            },
            Err(_) => warn!(
                proxy = %proxy_log_target(proxy),
                "Ignoring invalid all-proxy configuration"
            ),
        }
    }
    builder
}

pub(super) fn uri_host(uri: &str) -> Option<String> {
    reqwest::Url::parse(uri).ok()?.host_str().map(str::to_owned)
}

pub(crate) fn http_connect_timeout(options: &DownloadOptions) -> Duration {
    Duration::from_secs(
        options
            .connect_timeout
            .unwrap_or(constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS),
    )
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
    let connect_timeout = http_connect_timeout(options);
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
                .connect_timeout(connect_timeout)
                .gzip(options.http_accept_gzip)
                .user_agent(constants::USER_AGENT)
                .redirect(reqwest::redirect::Policy::none())
                .resolve_to_addrs(&host, addresses);
            let builder = apply_dns_resolver(builder, resolved_addresses.resolver.as_ref());
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

        if has_custom_tls || resolved_addresses.resolver.is_some() {
            let builder = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
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
            let builder = apply_dns_resolver(builder, resolved_addresses.resolver.as_ref());
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

        if resolved_addresses.resolver.is_none() {
            return Ok(
                crate::http::client_pool::get_bound_client_with_version_and_timeout(
                    local_address,
                    options.http_accept_gzip,
                    options.http_version,
                    connect_timeout,
                ),
            );
        }
    }

    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
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

    builder = apply_proxy_options(builder, &options.http_proxy_options());
    builder = apply_dns_resolver(builder, resolved_addresses.resolver.as_ref());

    if let (Some((proxy_host, _)), Some(proxy_addresses)) = (
        http_proxy_origin(uri, options),
        resolved_addresses
            .proxy
            .as_deref()
            .filter(|addresses| !addresses.is_empty()),
    ) {
        builder = builder.resolve_to_addrs(&proxy_host, proxy_addresses);
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

fn apply_dns_resolver(
    builder: reqwest::ClientBuilder,
    resolver: Option<&Arc<dyn reqwest::dns::Resolve>>,
) -> reqwest::ClientBuilder {
    match resolver {
        Some(resolver) => builder.dns_resolver(Arc::clone(resolver)),
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
    let mut proxy = if let Some(no_proxy) = no_proxy {
        // Reqwest's built-in NO_PROXY rules treat an unprefixed domain as a
        // suffix, while aria2's matcher treats it as an exact hostname. Use
        // the same matcher for proxy selection and actual request routing.
        match target {
            ProxyTarget::Http => reqwest::Proxy::http(proxy_url)?,
            ProxyTarget::Https => reqwest::Proxy::https(proxy_url)?,
            ProxyTarget::All => reqwest::Proxy::all(proxy_url)?,
        };
        let no_proxy = NoProxyMatcher::from_env_value(no_proxy);
        let proxy_url = proxy_url.to_owned();
        reqwest::Proxy::custom(move |url| {
            let scheme_matches = match target {
                ProxyTarget::Http => url.scheme() == "http",
                ProxyTarget::Https => url.scheme() == "https",
                ProxyTarget::All => matches!(url.scheme(), "http" | "https"),
            };
            let bypass = url
                .host_str()
                .is_some_and(|host| no_proxy.should_bypass_hostname(host));
            (scheme_matches && !bypass).then(|| proxy_url.clone())
        })
    } else {
        match target {
            ProxyTarget::Http => reqwest::Proxy::http(proxy_url)?,
            ProxyTarget::Https => reqwest::Proxy::https(proxy_url)?,
            ProxyTarget::All => reqwest::Proxy::all(proxy_url)?,
        }
    };

    // Preserve credentials embedded in the proxy URL unless an option
    // explicitly overrides them, matching AbstractCommand::makeProxyUri().
    if username.is_some() || password.is_some() {
        let embedded = ProxyUrl::parse(proxy_url).ok();
        let embedded_user = embedded
            .as_ref()
            .and_then(|proxy| proxy.username.as_deref())
            .map(str::to_string);
        let embedded_password = embedded.and_then(|proxy| proxy.password);
        let effective_user = username.map(str::to_owned).or(embedded_user);
        let effective_password = password
            .map(str::to_owned)
            .or(embedded_password)
            .unwrap_or_default();

        if let Some(user) = effective_user {
            proxy = proxy.basic_auth(&user, &effective_password);
        }
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
            warn!(
                proxy = %proxy_log_target(proxy_url),
                ?target,
                error = %error.without_url(),
                "Ignoring invalid HTTP proxy configuration"
            );
            builder
        }
    }
}

fn proxy_log_target(proxy_url: &str) -> String {
    let Ok(proxy) = ProxyUrl::parse(proxy_url) else {
        return "<invalid proxy URL>".to_string();
    };
    let scheme = match proxy.protocol {
        ProxyProtocol::Http => "http",
        ProxyProtocol::Https => "https",
        ProxyProtocol::Socks4 => "socks4",
        ProxyProtocol::Socks5 => "socks5",
    };
    let host = if proxy.host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{}]", proxy.host)
    } else {
        proxy.host
    };

    format!("{scheme}://{host}:{}", proxy.port)
}

#[cfg(test)]
mod tests;
