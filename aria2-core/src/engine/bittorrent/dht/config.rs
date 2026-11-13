//! Translate process DHT options and network policy into protocol configuration.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::parse_integer_segments;
use crate::dns::dns_cache::DnsCache;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::DownloadOptions;

/// The subset of process options that configures the shared DHT engines.
#[derive(Clone, Debug)]
pub(crate) struct DhtEngineOptions {
    pub(crate) async_dns: bool,
    pub(crate) dht_listen_port: Option<String>,
    pub(crate) dht_listen_addr: Option<String>,
    pub(crate) dht_listen_addr6: Option<String>,
    pub(crate) dht_entry_point: Option<Vec<String>>,
    pub(crate) dht_entry_point_host: Option<String>,
    pub(crate) dht_entry_point_port: Option<u16>,
    pub(crate) dht_entry_point6: Option<String>,
    pub(crate) dht_entry_point_host6: Option<String>,
    pub(crate) dht_entry_point_port6: Option<u16>,
    pub(crate) dht_file_path: Option<String>,
    pub(crate) dht_file_path6: Option<String>,
    pub(crate) dht_message_timeout: u64,
    pub(crate) dht_refresh_check_interval: u64,
    pub(crate) dht_token_rotation_interval: u64,
    pub(crate) dht_node_contact_interval: u64,
    pub(crate) dht_cleanup_interval: u64,
    pub(crate) dht_save_interval: u64,
    pub(crate) dht_bootstrap_timeout: u64,
    pub(crate) dht_max_concurrent_lookups: usize,
    pub(crate) dht_persistence_max_age: u64,
    pub(crate) dht_max_peer_info_hashes: usize,
    pub(crate) dht_max_peers_per_info_hash: usize,
    pub(crate) dht_max_items: usize,
}

impl From<&DownloadOptions> for DhtEngineOptions {
    fn from(options: &DownloadOptions) -> Self {
        let peer_storage_defaults =
            aria2_protocol::bittorrent::dht::DhtPeerStorageConfig::default();
        Self {
            async_dns: options.async_dns,
            dht_listen_port: options.dht_listen_port.clone(),
            dht_listen_addr: options.dht_listen_addr.clone(),
            dht_listen_addr6: options.dht_listen_addr6.clone(),
            dht_entry_point: options.dht_entry_point.clone(),
            dht_entry_point_host: options.dht_entry_point_host.clone(),
            dht_entry_point_port: options.dht_entry_point_port,
            dht_entry_point6: options.dht_entry_point6.clone(),
            dht_entry_point_host6: options.dht_entry_point_host6.clone(),
            dht_entry_point_port6: options.dht_entry_point_port6,
            dht_file_path: options.dht_file_path.clone(),
            dht_file_path6: options.dht_file_path6.clone(),
            dht_message_timeout: options.dht_message_timeout,
            dht_refresh_check_interval: options.dht_refresh_check_interval,
            dht_token_rotation_interval: options.dht_token_rotation_interval,
            dht_node_contact_interval: options.dht_node_contact_interval,
            dht_cleanup_interval: options.dht_cleanup_interval,
            dht_save_interval: options.dht_save_interval,
            dht_bootstrap_timeout: options.dht_bootstrap_timeout,
            dht_max_concurrent_lookups: options.dht_max_concurrent_lookups,
            dht_persistence_max_age: options.dht_persistence_max_age,
            dht_max_peer_info_hashes: peer_storage_defaults.max_info_hashes,
            dht_max_peers_per_info_hash: peer_storage_defaults.max_peers_per_info_hash,
            dht_max_items: aria2_protocol::bittorrent::dht::DEFAULT_DHT_MAX_ITEMS,
        }
    }
}

impl Default for DhtEngineOptions {
    fn default() -> Self {
        Self::from(&DownloadOptions::default())
    }
}

impl DhtEngineOptions {
    pub(crate) fn protocol_peer_storage_config(
        &self,
    ) -> aria2_protocol::bittorrent::dht::DhtPeerStorageConfig {
        aria2_protocol::bittorrent::dht::DhtPeerStorageConfig {
            max_info_hashes: self.dht_max_peer_info_hashes,
            max_peers_per_info_hash: self.dht_max_peers_per_info_hash,
        }
    }
}

/// Convert DHT options into the protocol engine configuration.
///
/// IPv4 and IPv6 use the corresponding address, bootstrap, and persistence
/// fields. Hostname bootstrap entries are resolved before the UDP engine is
/// started so invalid configuration is reported during engine startup.
#[cfg(test)]
pub(crate) async fn build_dht_engine_config(
    options: &DownloadOptions,
) -> Result<aria2_protocol::bittorrent::dht::engine::DhtEngineConfig> {
    let dns_cache = tokio::sync::Mutex::new(DnsCache::new());
    let dht_options = DhtEngineOptions::from(options);
    build_dht_engine_config_for_family_with_dns_cache(
        &dht_options,
        &crate::network::OutboundNetworkPolicy::direct(),
        &dns_cache,
        options.enable_dht6,
    )
    .await?
    .ok_or_else(|| config_error("no compatible outbound source for DHT".to_string()))
}

#[cfg(test)]
pub(crate) async fn build_dht_engine_config_with_policy(
    options: &DownloadOptions,
    policy: &crate::network::OutboundNetworkPolicy,
) -> Result<aria2_protocol::bittorrent::dht::engine::DhtEngineConfig> {
    let dns_cache = tokio::sync::Mutex::new(DnsCache::new());
    build_dht_engine_config_for_family_with_dns_cache(
        &DhtEngineOptions::from(options),
        policy,
        &dns_cache,
        options.enable_dht6,
    )
    .await?
    .ok_or_else(|| config_error("no compatible outbound source for DHT".to_string()))
}

#[cfg(test)]
pub(crate) async fn build_dht_engine_config_for_family(
    options: &DhtEngineOptions,
    policy: &crate::network::OutboundNetworkPolicy,
    use_ipv6: bool,
) -> Result<Option<aria2_protocol::bittorrent::dht::engine::DhtEngineConfig>> {
    let dns_cache = tokio::sync::Mutex::new(DnsCache::new());
    build_dht_engine_config_for_family_with_dns_cache(options, policy, &dns_cache, use_ipv6).await
}

pub(crate) async fn build_dht_engine_config_for_family_with_dns_cache(
    options: &DhtEngineOptions,
    policy: &crate::network::OutboundNetworkPolicy,
    dns_cache: &tokio::sync::Mutex<DnsCache>,
    use_ipv6: bool,
) -> Result<Option<aria2_protocol::bittorrent::dht::engine::DhtEngineConfig>> {
    let port_range = options
        .dht_listen_port
        .as_deref()
        .map(|value| {
            parse_integer_segments(value, 1024, u16::MAX as i64).map(|ranges| {
                ranges
                    .into_iter()
                    .flat_map(|range| range.map(|port| port as u16))
                    .collect::<Vec<_>>()
            })
        })
        .transpose()
        .map_err(|error| config_error(format!("invalid dht-listen-port: {error}")))?;

    let requested_listen_addr = selected_listen_addr(options, use_ipv6)?;
    let listen_addr = if policy.is_direct() {
        requested_listen_addr.or_else(|| use_ipv6.then_some(IpAddr::V6(Ipv6Addr::UNSPECIFIED)))
    } else {
        let addresses = policy.addresses();
        if let Some(address) = requested_listen_addr {
            if !addresses.contains(&address) {
                return Err(config_error(format!(
                    "DHT listen address '{address}' is not an outbound source address"
                )));
            }
            Some(address)
        } else if let Some(address) = addresses
            .into_iter()
            .find(|address| address.is_ipv6() == use_ipv6)
        {
            Some(address)
        } else {
            return Ok(None);
        }
    };
    let bootstrap_specs = selected_bootstrap_specs(options, use_ipv6)?;
    let (bootstrap_nodes, bootstrap_on_start) = if bootstrap_specs.is_empty() {
        if !options.async_dns || dns_cache.lock().await.dns_server_addresses().is_empty() {
            // Let the protocol engine perform its background system-DNS
            // bootstrap when async DNS is disabled or no explicit resolver is
            // configured.
            (Vec::new(), true)
        } else {
            let nodes = resolve_default_bootstrap_nodes(dns_cache, use_ipv6).await;
            let bootstrap_available = !nodes.is_empty();
            if !bootstrap_available {
                tracing::warn!(
                    ipv6 = use_ipv6,
                    "No default DHT bootstrap endpoint resolved using explicitly configured DNS servers; starting without public bootstrap"
                );
            }
            (nodes, bootstrap_available)
        }
    } else {
        (
            resolve_bootstrap_nodes(dns_cache, &bootstrap_specs, options.async_dns, use_ipv6)
                .await?,
            true,
        )
    };
    let dht_file_path = selected_file_path(options, use_ipv6)
        .map(|path| expand_home_path(path, home_dir().as_deref()))
        .or_else(|| Some(default_dht_file_path(use_ipv6)));

    Ok(Some(
        aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
            port: port_range
                .as_ref()
                .and_then(|ports| ports.first().copied())
                .unwrap_or(0),
            port_range,
            listen_addr,
            bootstrap_nodes,
            bootstrap_on_start,
            dht_file_path,
            query_timeout: Duration::from_secs(options.dht_message_timeout.max(1)),
            refresh_check_interval: Duration::from_secs(options.dht_refresh_check_interval.max(1)),
            token_rotation_interval: Duration::from_secs(
                options.dht_token_rotation_interval.max(1),
            ),
            node_contact_interval: Duration::from_secs(options.dht_node_contact_interval.max(1)),
            cleanup_interval: Duration::from_secs(options.dht_cleanup_interval.max(1)),
            save_interval: Duration::from_secs(options.dht_save_interval.max(1)),
            bootstrap_timeout: Duration::from_secs(options.dht_bootstrap_timeout.max(1)),
            max_concurrent_lookups: options.dht_max_concurrent_lookups.max(1),
            persistence_max_age: Duration::from_secs(options.dht_persistence_max_age.max(1)),
            max_stored_items: options.dht_max_items.max(1),
            ..Default::default()
        },
    ))
}

fn selected_listen_addr(options: &DhtEngineOptions, use_ipv6: bool) -> Result<Option<IpAddr>> {
    let raw = if use_ipv6 {
        options.dht_listen_addr6.as_deref()
    } else {
        options.dht_listen_addr.as_deref()
    };
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };

    let address = raw
        .parse::<IpAddr>()
        .map_err(|error| config_error(format!("invalid DHT listen address '{raw}': {error}")))?;
    if address.is_ipv6() != use_ipv6 {
        return Err(config_error(format!(
            "DHT listen address '{raw}' does not match the selected {} transport",
            if use_ipv6 { "IPv6" } else { "IPv4" }
        )));
    }
    Ok(Some(address))
}

fn selected_file_path(options: &DhtEngineOptions, use_ipv6: bool) -> Option<&str> {
    if use_ipv6 {
        options.dht_file_path6.as_deref()
    } else {
        options.dht_file_path.as_deref()
    }
}

fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return Some(PathBuf::from(home));
    }

    match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
        (Some(drive), Some(path)) => Some(Path::new(&drive).join(path)),
        _ => None,
    }
}

fn expand_home_path(value: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(value);
    };
    let home = home.to_string_lossy();
    PathBuf::from(value.replace("${HOME}", &home))
}

fn default_dht_file_path(use_ipv6: bool) -> PathBuf {
    let filename = if use_ipv6 { "dht6.dat" } else { "dht.dat" };
    let Some(home) = home_dir() else {
        return PathBuf::from(filename);
    };

    // Match aria2's migration behavior: retain the legacy file when it is
    // already present, otherwise use the XDG cache location.
    let legacy = home.join(".aria2").join(filename);
    if legacy.is_file() {
        return legacy;
    }

    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    cache.join("aria2").join(filename)
}

fn selected_bootstrap_specs(options: &DhtEngineOptions, use_ipv6: bool) -> Result<Vec<String>> {
    let mut specs = if use_ipv6 {
        options
            .dht_entry_point6
            .as_deref()
            .map(|value| vec![value.to_string()])
            .unwrap_or_default()
    } else {
        options.dht_entry_point.clone().unwrap_or_default()
    };

    let (host, port) = if use_ipv6 {
        (
            options.dht_entry_point_host6.as_deref(),
            options.dht_entry_point_port6,
        )
    } else {
        (
            options.dht_entry_point_host.as_deref(),
            options.dht_entry_point_port,
        )
    };
    if let Some(host) = host {
        let Some(port) = port.filter(|port| *port > 0) else {
            return Err(config_error(format!(
                "DHT bootstrap host '{host}' requires a non-zero port"
            )));
        };
        let endpoint = if use_ipv6 && host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        specs.push(endpoint);
    }

    Ok(specs)
}

async fn resolve_bootstrap_nodes(
    dns_cache: &tokio::sync::Mutex<DnsCache>,
    specs: &[String],
    async_dns: bool,
    use_ipv6: bool,
) -> Result<Vec<SocketAddr>> {
    let mut nodes = Vec::with_capacity(specs.len());
    for spec in specs {
        let (host, port) = parse_bootstrap_endpoint(spec)
            .ok_or_else(|| config_error(format!("invalid DHT bootstrap endpoint '{spec}'")))?;
        let addresses = resolve_bootstrap_host(dns_cache, host, port, async_dns)
            .await
            .map_err(|error| {
                config_error(format!("cannot resolve DHT bootstrap '{spec}': {error}"))
            })?;
        let mut addresses = addresses.into_iter();
        let address = addresses
            .find(|address| address.is_ipv6() == use_ipv6)
            .ok_or_else(|| {
                config_error(format!(
                    "DHT bootstrap '{spec}' has no matching {} address",
                    if use_ipv6 { "IPv6" } else { "IPv4" }
                ))
            })?;
        nodes.push(address);
    }
    Ok(nodes)
}

async fn resolve_default_bootstrap_nodes(
    dns_cache: &tokio::sync::Mutex<DnsCache>,
    use_ipv6: bool,
) -> Vec<SocketAddr> {
    let mut nodes = Vec::new();
    for endpoint in aria2_protocol::bittorrent::dht::bootstrap::DhtBootstrap::bootstrap_node_list()
    {
        let Some((host, port)) = parse_bootstrap_endpoint(&endpoint) else {
            continue;
        };
        match resolve_bootstrap_host(dns_cache, host, port, true).await {
            Ok(addresses) => {
                if let Some(address) = addresses
                    .into_iter()
                    .find(|address| address.is_ipv6() == use_ipv6)
                {
                    nodes.push(address);
                }
            }
            Err(error) => {
                tracing::warn!(%error, endpoint, "Failed to resolve default DHT bootstrap endpoint")
            }
        }
    }
    nodes
}

async fn resolve_bootstrap_host(
    dns_cache: &tokio::sync::Mutex<DnsCache>,
    host: &str,
    port: u16,
    async_dns: bool,
) -> Result<Vec<SocketAddr>> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    if async_dns {
        dns_cache
            .lock()
            .await
            .resolve_with_refresh(host, port)
            .await
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map(|addresses| addresses.collect())
            .map_err(|error| Aria2Error::NameResolve(error.to_string()))
    }
}

fn parse_bootstrap_endpoint(endpoint: &str) -> Option<(&str, u16)> {
    let (host, port) = endpoint.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    (!host.is_empty()).then_some((host, port))
}

fn config_error(message: String) -> Aria2Error {
    Aria2Error::Fatal(FatalError::Config(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ipv6_dht_options_enter_the_protocol_engine_config() {
        let options = DownloadOptions {
            enable_dht6: true,
            dht_listen_port: Some("49001-49002".to_string()),
            dht_listen_addr6: Some("::1".to_string()),
            dht_entry_point6: Some("[::1]:49003".to_string()),
            dht_file_path6: Some("dht6.dat".to_string()),
            dht_message_timeout: 7,
            ..Default::default()
        };

        let config = build_dht_engine_config(&options)
            .await
            .expect("IPv6 DHT options should build a protocol config");

        assert_eq!(config.listen_addr, Some("::1".parse().unwrap()));
        assert_eq!(config.port, 49001);
        assert_eq!(config.port_range, Some(vec![49001, 49002]));
        assert_eq!(
            config.bootstrap_nodes,
            vec!["[::1]:49003".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(
            config.dht_file_path.as_deref(),
            Some(std::path::Path::new("dht6.dat"))
        );
        assert_eq!(config.query_timeout, Duration::from_secs(7));
    }

    #[tokio::test]
    async fn process_dht_tuning_options_enter_the_protocol_engine_config() {
        let options = DownloadOptions {
            dht_message_timeout: 11,
            dht_refresh_check_interval: 301,
            dht_token_rotation_interval: 601,
            dht_node_contact_interval: 901,
            dht_cleanup_interval: 302,
            dht_save_interval: 1801,
            dht_bootstrap_timeout: 61,
            dht_max_concurrent_lookups: 23,
            dht_persistence_max_age: 86_401,
            ..Default::default()
        };

        let config = build_dht_engine_config(&options)
            .await
            .expect("DHT tuning options should build a protocol config");

        assert_eq!(config.query_timeout, Duration::from_secs(11));
        assert_eq!(config.refresh_check_interval, Duration::from_secs(301));
        assert_eq!(config.token_rotation_interval, Duration::from_secs(601));
        assert_eq!(config.node_contact_interval, Duration::from_secs(901));
        assert_eq!(config.cleanup_interval, Duration::from_secs(302));
        assert_eq!(config.save_interval, Duration::from_secs(1801));
        assert_eq!(config.bootstrap_timeout, Duration::from_secs(61));
        assert_eq!(config.max_concurrent_lookups, 23);
        assert_eq!(config.persistence_max_age, Duration::from_secs(86_401));
        let mut dht_options = DhtEngineOptions::from(&options);
        dht_options.dht_max_peer_info_hashes = 1200;
        dht_options.dht_max_peers_per_info_hash = 37;
        dht_options.dht_max_items = 512;
        let configured = build_dht_engine_config_for_family(
            &dht_options,
            &crate::network::OutboundNetworkPolicy::direct(),
            false,
        )
        .await
        .expect("DHT options should map into protocol config")
        .expect("IPv4 DHT config should exist");
        assert_eq!(configured.max_stored_items, 512);
        assert_eq!(
            dht_options.protocol_peer_storage_config(),
            aria2_protocol::bittorrent::dht::DhtPeerStorageConfig {
                max_info_hashes: 1200,
                max_peers_per_info_hash: 37,
            }
        );
    }

    #[tokio::test]
    async fn dual_stack_policy_selects_matching_dht_udp_source_without_explicit_listen_addr() {
        let options6 = DownloadOptions {
            enable_dht6: true,
            dht_entry_point6: Some("[::1]:49003".to_string()),
            ..Default::default()
        };
        let policy = crate::network::OutboundNetworkPolicy::new(vec![
            "127.0.0.2".parse().expect("parse IPv4 source"),
            "::1".parse().expect("parse IPv6 source"),
        ])
        .expect("dual-stack policy should build");

        let config6 = build_dht_engine_config_with_policy(&options6, &policy)
            .await
            .expect("DHT6 config should select a compatible policy source");

        assert_eq!(config6.listen_addr, Some("::1".parse().unwrap()));

        let options4 = DownloadOptions {
            dht_entry_point: Some(vec!["127.0.0.1:49004".to_string()]),
            ..Default::default()
        };
        let config4 = build_dht_engine_config_with_policy(&options4, &policy)
            .await
            .expect("DHT4 config should select a compatible policy source");

        assert_eq!(config4.listen_addr, Some("127.0.0.2".parse().unwrap()));
    }

    #[tokio::test]
    async fn dual_stack_options_build_independent_family_configs_and_snapshots() {
        let options = DownloadOptions {
            enable_dht: true,
            enable_dht6: true,
            dht_listen_addr: Some("127.0.0.1".to_string()),
            dht_listen_addr6: Some("::1".to_string()),
            dht_file_path: Some("v4.dat".to_string()),
            dht_file_path6: Some("v6.dat".to_string()),
            ..Default::default()
        };
        let policy = crate::network::OutboundNetworkPolicy::direct();

        let dht_options = DhtEngineOptions::from(&options);
        let config4 = build_dht_engine_config_for_family(&dht_options, &policy, false)
            .await
            .expect("IPv4 DHT options should build independently")
            .expect("direct IPv4 DHT should be available");
        let config6 = build_dht_engine_config_for_family(&dht_options, &policy, true)
            .await
            .expect("IPv6 DHT options should build independently")
            .expect("direct IPv6 DHT should be available");

        assert_eq!(config4.listen_addr, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(config6.listen_addr, Some("::1".parse().unwrap()));
        assert_eq!(config4.dht_file_path.as_deref(), Some(Path::new("v4.dat")));
        assert_eq!(config6.dht_file_path.as_deref(), Some(Path::new("v6.dat")));
    }

    #[tokio::test]
    async fn constrained_policy_skips_dht_for_an_unavailable_address_family() {
        let options = DownloadOptions {
            enable_dht6: true,
            ..Default::default()
        };
        let policy = crate::network::OutboundNetworkPolicy::single("::1".parse().unwrap());

        let dht_options = DhtEngineOptions::from(&options);
        let config4 = build_dht_engine_config_for_family(&dht_options, &policy, false)
            .await
            .expect("an unavailable address family should be skipped");
        let config6 = build_dht_engine_config_for_family(&dht_options, &policy, true)
            .await
            .expect("the configured address family should build");

        assert!(config4.is_none());
        assert_eq!(config6.unwrap().listen_addr, Some("::1".parse().unwrap()));
    }

    #[tokio::test]
    async fn constrained_policy_rejects_a_conflicting_explicit_dht_address() {
        let options = DownloadOptions {
            dht_listen_addr: Some("127.0.0.2".to_string()),
            ..Default::default()
        };
        let policy = crate::network::OutboundNetworkPolicy::single("127.0.0.1".parse().unwrap());

        let dht_options = DhtEngineOptions::from(&options);
        let error = build_dht_engine_config_for_family(&dht_options, &policy, false)
            .await
            .expect_err("DHT must not bind outside the selected source address");

        assert!(
            matches!(error, Aria2Error::Fatal(FatalError::Config(message)) if message.contains("not an outbound source address"))
        );
    }

    #[tokio::test]
    async fn explicit_dns_failure_disables_default_dht_bootstrap_without_system_fallback() {
        let options = DownloadOptions::default();
        let dht_options = DhtEngineOptions::from(&options);
        let policy = crate::network::OutboundNetworkPolicy::direct();
        let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
            Duration::from_millis(200),
            vec!["127.0.0.1:9".parse().unwrap()],
        ));

        let config = build_dht_engine_config_for_family_with_dns_cache(
            &dht_options,
            &policy,
            &dns_cache,
            false,
        )
        .await
        .expect("unavailable default bootstrap servers must not fail the download")
        .expect("direct DHT should remain available");

        assert!(config.bootstrap_nodes.is_empty());
        assert!(!config.bootstrap_on_start);
    }

    #[tokio::test]
    async fn explicit_dns_resolves_default_dht_bootstrap_endpoints() {
        use tokio::net::UdpSocket;

        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("DNS fixture should bind");
        let server_address = socket.local_addr().expect("DNS fixture address");
        let dns_server = tokio::spawn(async move {
            let mut request = [0_u8; 512];
            loop {
                let Ok((length, peer)) = socket.recv_from(&mut request).await else {
                    break;
                };
                let request = &request[..length];
                let mut question_end = 12;
                while question_end < request.len() && request[question_end] != 0 {
                    question_end += 1;
                }
                question_end += 5;
                if question_end > request.len() {
                    continue;
                }
                let query_type =
                    u16::from_be_bytes([request[question_end - 4], request[question_end - 3]]);
                let has_ipv4_answer = query_type == 1;
                let mut response = Vec::with_capacity(question_end + 16);
                response.extend_from_slice(&request[..2]);
                response.extend_from_slice(&[
                    0x81,
                    0x80,
                    0,
                    1,
                    0,
                    u8::from(has_ipv4_answer),
                    0,
                    0,
                    0,
                    0,
                ]);
                response.extend_from_slice(&request[12..question_end]);
                if has_ipv4_answer {
                    response.extend_from_slice(&[
                        0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1,
                    ]);
                }
                let _ = socket.send_to(&response, peer).await;
            }
        });

        let options = DownloadOptions::default();
        let dht_options = DhtEngineOptions::from(&options);
        let policy = crate::network::OutboundNetworkPolicy::direct();
        let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
            Duration::from_secs(2),
            vec![server_address],
        ));

        let config = build_dht_engine_config_for_family_with_dns_cache(
            &dht_options,
            &policy,
            &dns_cache,
            false,
        )
        .await
        .expect("DHT config should resolve through its explicit DNS server")
        .expect("direct DHT should be available");
        dns_server.abort();

        assert!(config.bootstrap_on_start);
        assert!(!config.bootstrap_nodes.is_empty());
        assert!(
            config
                .bootstrap_nodes
                .iter()
                .all(|address| address.ip().is_loopback() && address.port() == 6881)
        );
    }

    #[tokio::test]
    async fn async_dns_disabled_uses_system_resolution_for_custom_bootstrap() {
        let options = DownloadOptions {
            async_dns: false,
            dht_entry_point: Some(vec!["localhost:49003".to_string()]),
            ..Default::default()
        };
        let dht_options = DhtEngineOptions::from(&options);
        let policy = crate::network::OutboundNetworkPolicy::direct();
        let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
            Duration::from_millis(200),
            vec!["127.0.0.1:9".parse().unwrap()],
        ));

        let config = build_dht_engine_config_for_family_with_dns_cache(
            &dht_options,
            &policy,
            &dns_cache,
            false,
        )
        .await
        .expect("system resolver should resolve localhost when async DNS is disabled")
        .expect("direct DHT should be available");

        assert_eq!(config.bootstrap_nodes.len(), 1);
        assert!(config.bootstrap_nodes[0].ip().is_loopback());
        assert_eq!(config.bootstrap_nodes[0].port(), 49003);

        let default_options = DhtEngineOptions::from(&DownloadOptions {
            async_dns: false,
            ..Default::default()
        });
        let default_config = build_dht_engine_config_for_family_with_dns_cache(
            &default_options,
            &policy,
            &dns_cache,
            false,
        )
        .await
        .expect("default bootstrap should keep the system resolver path")
        .expect("direct DHT should be available");
        assert!(default_config.bootstrap_nodes.is_empty());
        assert!(default_config.bootstrap_on_start);
    }

    #[tokio::test]
    async fn separate_ipv4_host_and_port_enter_the_bootstrap_list() {
        let options = DownloadOptions {
            dht_entry_point_host: Some("127.0.0.1".to_string()),
            dht_entry_point_port: Some(49004),
            ..Default::default()
        };

        let config = build_dht_engine_config(&options)
            .await
            .expect("IPv4 DHT host and port should build a protocol config");

        assert_eq!(
            config.bootstrap_nodes,
            vec!["127.0.0.1:49004".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn ipv6_path_does_not_fall_back_to_ipv4_path() {
        let options = DownloadOptions {
            dht_file_path: Some("v4.dat".to_string()),
            ..Default::default()
        };
        let options = DhtEngineOptions::from(&options);

        assert_eq!(selected_file_path(&options, true), None);
        assert_eq!(selected_file_path(&options, false), Some("v4.dat"));
    }

    #[test]
    fn home_placeholder_expands_in_dht_paths() {
        let expanded = expand_home_path("${HOME}/.aria2/dht.dat", Some(Path::new("/srv/user")));
        assert_eq!(expanded, PathBuf::from("/srv/user/.aria2/dht.dat"));
    }
}
