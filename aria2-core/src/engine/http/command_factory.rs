//! Async assembly of an HTTP command from a request group and shared engine
//! services.
//!
//! DNS resolution belongs here, beside the HTTP client configuration that
//! consumes its results. The generic task dispatcher only selects this
//! protocol factory.

use std::sync::Arc;

use crate::dns::dns_cache::DnsCache;
use crate::error::Result;
use crate::network::OutboundNetworkPolicy;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::client_config::{ResolvedNetworkAddresses, http_proxy_origin};
use super::download_command::DownloadCommand;

pub(crate) async fn create_download_command(
    group: Arc<std::sync::RwLock<RequestGroup>>,
    uri: &str,
    options: &DownloadOptions,
    dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,
    outbound_network_policy: Arc<OutboundNetworkPolicy>,
    global_limiter: Option<RateLimiter>,
) -> Result<DownloadCommand> {
    let proxy_origin = http_proxy_origin(uri, options);
    let resolved_target = if proxy_origin.is_none() {
        if let Some((hostname, port)) = http_origin(uri) {
            if let Ok(address) = hostname.parse::<std::net::IpAddr>() {
                options
                    .async_dns
                    .then(|| vec![std::net::SocketAddr::new(address, port)])
            } else if options.async_dns {
                resolve_cached_endpoint(&dns_cache, &hostname, port).await?
            } else if !outbound_network_policy.is_direct() {
                resolve_system_endpoint(&hostname, port).await
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let resolved_proxy = if let Some((hostname, port)) = proxy_origin {
        if let Ok(address) = hostname.parse::<std::net::IpAddr>() {
            Some(vec![std::net::SocketAddr::new(address, port)])
        } else if options.async_dns {
            resolve_cached_endpoint(&dns_cache, &hostname, port).await?
        } else if !outbound_network_policy.is_direct() {
            resolve_system_endpoint(&hostname, port).await
        } else {
            None
        }
    } else {
        None
    };

    let output_dir = options.dir.as_deref();
    let group_output_name = group.recover().output_name();
    let output_name = group_output_name.as_deref().or(options.out.as_deref());
    let mut command = DownloadCommand::new_with_group_and_resolved_network_addresses(
        group,
        uri,
        options,
        output_dir,
        output_name,
        ResolvedNetworkAddresses {
            target: resolved_target,
            proxy: resolved_proxy,
        },
        outbound_network_policy,
    )?;
    if let Some(limiter) = global_limiter {
        command.set_global_limiter(limiter);
    }
    Ok(command)
}

async fn resolve_cached_endpoint(
    dns_cache: &tokio::sync::Mutex<DnsCache>,
    hostname: &str,
    port: u16,
) -> Result<Option<Vec<std::net::SocketAddr>>> {
    let mut dns_cache = dns_cache.lock().await;
    let has_explicit_servers = !dns_cache.dns_server_addresses().is_empty();
    match dns_cache.resolve_with_refresh(hostname, port).await {
        Ok(addresses) => Ok(Some(addresses)),
        Err(error) if has_explicit_servers => Err(error),
        // With system DNS, preserve reqwest's own fallback when the shared
        // cache and its system lookup path cannot produce an address.
        Err(_) => Ok(None),
    }
}

async fn resolve_system_endpoint(hostname: &str, port: u16) -> Option<Vec<std::net::SocketAddr>> {
    tokio::net::lookup_host((hostname, port))
        .await
        .ok()
        .map(|addresses| addresses.collect())
}

fn http_origin(uri: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(uri).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some((
        parsed.host_str()?.to_owned(),
        parsed.port_or_known_default()?,
    ))
}
