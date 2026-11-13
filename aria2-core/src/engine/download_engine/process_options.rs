use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::DownloadEngine;
use crate::config::OptionValue;
use crate::dns::dns_cache::DnsCache;
use crate::network::OutboundNetworkPolicy;
use crate::rate_limiter::RateLimiterConfig;
#[cfg(feature = "bittorrent")]
use crate::request::request_group::DownloadOptions;
use crate::request::request_group_man::RequestGroupMan;

impl DownloadEngine {
    /// Construct an engine with process-wide settings applied before its loop
    /// starts and before protocol-owned sockets are created.
    ///
    /// The CLI, C API, and Rust library callers use this boundary to apply
    /// protocol-facing transports, resolver settings, scheduler limits, and
    /// server statistics before any process-owned services start.
    pub async fn from_process_options(
        options: &HashMap<String, OptionValue>,
        request_man: Arc<RequestGroupMan>,
    ) -> Self {
        let outbound_spec = option_string(options, "interface")
            .or_else(|| option_string(options, "multiple-interface"));
        let outbound_policy = match outbound_spec {
            Some(spec) => match OutboundNetworkPolicy::resolve_spec(spec).await {
                Ok(policy) => Arc::new(policy),
                Err(error) => {
                    tracing::warn!(%error, %spec, "Ignoring invalid network interface configuration");
                    Arc::new(OutboundNetworkPolicy::direct())
                }
            },
            None => Arc::new(OutboundNetworkPolicy::direct()),
        };
        #[cfg(feature = "bittorrent")]
        let mut engine = Self::with_lpd_manager(build_lpd_manager(options, &outbound_policy));
        #[cfg(not(feature = "bittorrent"))]
        let mut engine = Self::new();
        engine.set_outbound_network_policy(Arc::clone(&outbound_policy));

        let dns_timeout = option_i64(options, "dns-timeout")
            .filter(|seconds| *seconds > 0)
            .map(|seconds| Duration::from_secs(seconds as u64))
            .unwrap_or(Duration::from_secs(30));
        let dns_servers = option_string(options, "async-dns-server").and_then(|value| {
            match DnsCache::parse_dns_server_list(value) {
                Ok(servers) => Some(servers),
                Err(error) => {
                    tracing::warn!(%error, "Ignoring invalid async-dns-server configuration");
                    None
                }
            }
        });
        engine.set_dns_config(dns_timeout, dns_servers);

        #[cfg(feature = "bittorrent")]
        engine.configure_bittorrent_process_options(options);

        let server_stat_timeout = option_i64(options, "server-stat-timeout")
            .filter(|seconds| *seconds >= 0)
            .map(|seconds| seconds as u64)
            .unwrap_or(24 * 60 * 60);
        let server_stat_input = option_string(options, "server-stat-if").map(PathBuf::from);
        let server_stat_output = option_string(options, "server-stat-of").map(PathBuf::from);
        let server_stat_interval = option_i64(options, "save-server-stat-interval")
            .filter(|seconds| *seconds > 0)
            .map(|seconds| Duration::from_secs(seconds as u64));
        engine.set_server_stat_timeout(server_stat_timeout);
        engine.set_server_stat_persistence(
            server_stat_input,
            server_stat_output,
            server_stat_interval,
        );

        if let Some(max) =
            option_i64(options, "max-concurrent-downloads").filter(|value| *value >= 0)
            && let Ok(max) = u32::try_from(max)
        {
            request_man.set_max_concurrent(max);
        }
        let download_limit =
            option_i64(options, "max-overall-download-limit").and_then(non_zero_limit);
        let upload_limit = option_i64(options, "max-overall-upload-limit").and_then(non_zero_limit);
        if download_limit.is_some() || upload_limit.is_some() {
            engine.set_global_rate_limiter(RateLimiterConfig::new(download_limit, upload_limit));
            request_man.set_global_speed_limit(download_limit, upload_limit);
        }
        engine.set_request_group_man(request_man);
        engine
    }

    #[cfg(feature = "bittorrent")]
    fn configure_bittorrent_process_options(&mut self, options: &HashMap<String, OptionValue>) {
        use crate::engine::bittorrent::tracker::source_fetcher::CoreTrackerSourceFetcher;
        use aria2_protocol::bittorrent::tracker::public_list::{
            DEFAULT_TRACKER_SOURCE_TIMEOUT, DEFAULT_TRACKER_UPDATE_INTERVAL,
            MAX_TRACKER_SOURCE_CONCURRENCY, TrackerCatalogConfig,
        };

        if let Some(max_open_files) = option_i64(options, "bt-max-open-files")
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
        {
            self.open_file_budget.set_max_open_files(max_open_files);
        }

        self.set_utp_socket_options(crate::config::utp_socket_options_from_values(
            option_i64(options, "bt-utp-max-connections"),
            option_i64(options, "bt-utp-connect-timeout"),
            option_i64(options, "bt-utp-idle-timeout"),
            option_i64(options, "bt-utp-keepalive-interval"),
        ));
        if let Some(seconds) = option_i64(options, "bt-incoming-peer-handshake-timeout")
            .and_then(|seconds| u64::try_from(seconds).ok())
        {
            self.set_incoming_peer_handshake_timeout(Duration::from_secs(seconds));
        }

        let sources = match options.get("bt-tracker-source") {
            Some(OptionValue::List(values)) => values.clone(),
            Some(OptionValue::Str(value)) => value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        let update_interval = option_i64(options, "bt-tracker-update-interval")
            .filter(|seconds| *seconds > 0)
            .map(|seconds| Duration::from_secs(seconds as u64))
            .unwrap_or(DEFAULT_TRACKER_UPDATE_INTERVAL);
        let source_timeout = option_i64(options, "bt-tracker-source-timeout")
            .and_then(|seconds| u64::try_from(seconds).ok())
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TRACKER_SOURCE_TIMEOUT);
        let tracker_config = TrackerCatalogConfig {
            enabled: option_bool(options, "enable-public-trackers").unwrap_or(true),
            sources,
            update_interval,
        };
        let option_strings = options
            .iter()
            .map(|(name, value)| (name.clone(), value.to_string()))
            .collect();
        let download_options = DownloadOptions::from_option_strings(&option_strings);
        let mut dht_options =
            crate::engine::bittorrent::dht::config::DhtEngineOptions::from(&download_options);
        let default_peer_storage = aria2_protocol::bittorrent::dht::DhtPeerStorageConfig::default();
        dht_options.dht_max_peer_info_hashes = option_usize(options, "dht-max-peer-info-hashes")
            .unwrap_or(default_peer_storage.max_info_hashes);
        dht_options.dht_max_peers_per_info_hash =
            option_usize(options, "dht-max-peers-per-info-hash")
                .unwrap_or(default_peer_storage.max_peers_per_info_hash);
        self.bt_registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_global_dht_options(Arc::new(dht_options));
        let source_fetcher = Arc::new(CoreTrackerSourceFetcher::new(
            Arc::clone(&self.dns_cache),
            Arc::clone(&self.outbound_network_policy),
            download_options,
        ));
        let tracker_catalog = Arc::new(
            aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new_with_config_and_fetcher(
                tracker_config,
                source_fetcher,
            ),
        );
        tracker_catalog.set_source_fetch_concurrency_limit(
            option_i64(options, "bt-tracker-source-concurrency")
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(MAX_TRACKER_SOURCE_CONCURRENCY),
        );
        self.public_tracker_catalog = tracker_catalog;
        self.set_public_tracker_source_timeout(source_timeout);
    }
}

#[cfg(feature = "bittorrent")]
fn build_lpd_manager(
    options: &HashMap<String, OptionValue>,
    outbound_policy: &OutboundNetworkPolicy,
) -> Arc<crate::engine::bittorrent::discovery::lpd::LpdManager> {
    use crate::engine::bittorrent::discovery::lpd::LpdManager;

    let lpd_port = option_i64(options, "lpd-listen-port")
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(crate::constants::LPD_PORT);
    let lpd_interface = match options.get("bt-lpd-interface") {
        Some(OptionValue::Str(value)) if !value.trim().is_empty() => {
            match value.parse::<std::net::Ipv4Addr>() {
                Ok(address) => Some(address),
                Err(error) => {
                    tracing::warn!(%error, %value, "Ignoring invalid bt-lpd-interface");
                    None
                }
            }
        }
        _ => None,
    }
    .or_else(|| {
        outbound_policy
            .addresses()
            .into_iter()
            .find_map(|address| match address {
                std::net::IpAddr::V4(address) => Some(address),
                std::net::IpAddr::V6(_) => None,
            })
    });
    match LpdManager::with_interval_and_interface_and_port(
        crate::constants::LPD_DEFAULT_ANNOUNCE_INTERVAL_SECS,
        lpd_interface,
        lpd_port,
    ) {
        Ok(manager) => Arc::new(manager),
        Err(error) => {
            tracing::warn!(%error, "Using default LPD manager because configured LPD setup failed");
            Arc::new(LpdManager::new())
        }
    }
}

fn non_zero_limit(value: i64) -> Option<u64> {
    (value > 0).then_some(value as u64)
}

fn option_i64(options: &HashMap<String, OptionValue>, name: &str) -> Option<i64> {
    match options.get(name)? {
        OptionValue::Int(value) => Some(*value),
        OptionValue::Usize(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

#[cfg(feature = "bittorrent")]
fn option_usize(options: &HashMap<String, OptionValue>, name: &str) -> Option<usize> {
    match options.get(name)? {
        OptionValue::Int(value) => usize::try_from(*value).ok(),
        OptionValue::Usize(value) => Some(*value),
        _ => None,
    }
}

#[cfg(all(test, feature = "bittorrent"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_options_initialize_shared_bt_and_dht_configuration() {
        let options = HashMap::from([
            ("bt-max-open-files".to_string(), OptionValue::Int(17)),
            (
                "bt-tracker-source-concurrency".to_string(),
                OptionValue::Int(2),
            ),
            ("dht-message-timeout".to_string(), OptionValue::Int(23)),
            (
                "dht-listen-port".to_string(),
                OptionValue::Str("49001-49002".to_string()),
            ),
            (
                "dht-file-path".to_string(),
                OptionValue::Str("process-dht.dat".to_string()),
            ),
            (
                "dht-max-peer-info-hashes".to_string(),
                OptionValue::Int(1200),
            ),
            (
                "dht-max-peers-per-info-hash".to_string(),
                OptionValue::Int(37),
            ),
        ]);
        let engine =
            DownloadEngine::from_process_options(&options, Arc::new(RequestGroupMan::new())).await;

        assert_eq!(engine.open_file_budget.max_open_files(), 17);
        assert_eq!(
            engine
                .public_tracker_catalog()
                .source_fetch_concurrency_limit(),
            2
        );
        let dht_options = engine
            .bt_registry
            .read()
            .expect("BT registry should be readable")
            .global_dht_options();
        assert_eq!(dht_options.dht_message_timeout, 23);
        assert_eq!(dht_options.dht_listen_port.as_deref(), Some("49001-49002"));
        assert_eq!(
            dht_options.dht_file_path.as_deref(),
            Some("process-dht.dat")
        );
        assert_eq!(dht_options.dht_max_peer_info_hashes, 1200);
        assert_eq!(dht_options.dht_max_peers_per_info_hash, 37);
    }
}

fn option_string<'a>(options: &'a HashMap<String, OptionValue>, name: &str) -> Option<&'a str> {
    match options.get(name)? {
        OptionValue::Str(value) => Some(value),
        _ => None,
    }
}

#[cfg(feature = "bittorrent")]
fn option_bool(options: &HashMap<String, OptionValue>, name: &str) -> Option<bool> {
    match options.get(name)? {
        OptionValue::Bool(value) => Some(*value),
        _ => None,
    }
}
