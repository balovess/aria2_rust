//! Build the protocol DHT configuration from one download option snapshot.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::config::parse_integer_segments;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::DownloadOptions;

/// Convert the task-level DHT options into the protocol engine configuration.
///
/// IPv4 and IPv6 use the corresponding address, bootstrap, and persistence
/// fields. Hostname bootstrap entries are resolved before the UDP engine is
/// started so invalid configuration is reported at the task boundary.
pub(crate) async fn build_dht_engine_config(
    options: &DownloadOptions,
) -> Result<aria2_protocol::bittorrent::dht::engine::DhtEngineConfig> {
    let use_ipv6 = options.enable_dht6;
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

    let listen_addr = selected_listen_addr(options, use_ipv6)?;
    let bootstrap_specs = selected_bootstrap_specs(options, use_ipv6)?;
    let bootstrap_nodes = resolve_bootstrap_nodes(&bootstrap_specs, use_ipv6).await?;
    let dht_file_path = selected_file_path(options, use_ipv6);

    Ok(aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
        port: port_range
            .as_ref()
            .and_then(|ports| ports.first().copied())
            .unwrap_or(0),
        port_range,
        listen_addr,
        bootstrap_nodes,
        dht_file_path: dht_file_path.map(std::path::PathBuf::from),
        query_timeout: Duration::from_secs(options.dht_message_timeout.max(1)),
        ..Default::default()
    })
}

fn selected_listen_addr(options: &DownloadOptions, use_ipv6: bool) -> Result<Option<IpAddr>> {
    let raw = if use_ipv6 {
        options.dht_listen_addr6.as_deref()
    } else {
        options.dht_listen_addr.as_deref()
    };
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(use_ipv6.then_some(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
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

fn selected_file_path(options: &DownloadOptions, use_ipv6: bool) -> Option<&str> {
    if use_ipv6 {
        options
            .dht_file_path6
            .as_deref()
            .or(options.dht_file_path.as_deref())
    } else {
        options.dht_file_path.as_deref()
    }
}

fn selected_bootstrap_specs(options: &DownloadOptions, use_ipv6: bool) -> Result<Vec<String>> {
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

async fn resolve_bootstrap_nodes(specs: &[String], use_ipv6: bool) -> Result<Vec<SocketAddr>> {
    let mut nodes = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut addresses = tokio::net::lookup_host(spec.as_str())
            .await
            .map_err(|error| {
                config_error(format!("cannot resolve DHT bootstrap '{spec}': {error}"))
            })?;
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
}
