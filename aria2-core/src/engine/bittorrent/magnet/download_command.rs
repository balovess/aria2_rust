use async_trait::async_trait;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::bittorrent::magnet::metadata_exchange::{
    MetadataExchangeConfig, MetadataExchangeSession,
};
use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;
use crate::engine::command::{Command, CommandStatus};
use crate::engine::download_event_hooks::{DownloadEventHooks, MetadataResolvedEvent};
use crate::engine::http::client_config::{ProxyTarget, add_reqwest_proxy};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::http::client_identity::ClientTlsConfig;
use crate::http::socks_connector::ProxyUrl;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

const MAX_MAGNET_TRACKERS_TO_TRY: usize = 10;
const MAX_MAGNET_TRACKER_PEERS: usize = 50;
const MAX_MAGNET_METADATA_PEERS_TO_TRY: usize = 20;

fn metadata_tracker_urls(
    magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    options: &DownloadOptions,
) -> Vec<String> {
    let excluded = options.bt_exclude_tracker.as_deref().unwrap_or_default();
    if excluded.iter().any(|url| url == "*") {
        return Vec::new();
    }

    let mut urls = magnet.trackers.clone();
    if let Some(overrides) = options.bt_tracker.as_ref() {
        urls.extend(overrides.iter().cloned());
    }

    urls.into_iter()
        .map(|url| url.trim().to_owned())
        .filter(|url| {
            !url.is_empty()
                && matches!(
                    reqwest::Url::parse(url)
                        .ok()
                        .as_ref()
                        .map(reqwest::Url::scheme),
                    Some("http" | "https" | "udp" | "ws" | "wss")
                )
                && !excluded.iter().any(|excluded| excluded == url)
        })
        .fold(Vec::new(), |mut unique, url| {
            if !unique.iter().any(|known| known == &url) {
                unique.push(url);
            }
            unique
        })
}

fn tracker_peer_socket_addr(ip: &str, port: u16) -> Option<SocketAddr> {
    ip.parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port))
}

fn append_unique_tracker_peers(discovered: &mut Vec<SocketAddr>, peers: Vec<SocketAddr>) {
    for peer in peers {
        if !discovered.contains(&peer) {
            discovered.push(peer);
            if discovered.len() >= MAX_MAGNET_TRACKER_PEERS {
                break;
            }
        }
    }
}

pub struct MagnetDownloadCommand {
    group: Arc<std::sync::RwLock<RequestGroup>>,
    magnet_uri: String,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    metadata_complete: bool,
    dht_engines: DhtEngineSet,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// Carried through to the internally-created `BtDownloadCommand`.
    global_limiter: Option<RateLimiter>,
    outbound_network_policy: Arc<crate::network::OutboundNetworkPolicy>,
    #[cfg(feature = "bittorrent")]
    public_tracker_catalog:
        Option<Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    #[cfg(feature = "bittorrent")]
    bt_listener: Option<Arc<crate::engine::bittorrent::peer::listener::BtPeerListenerManager>>,
    #[cfg(feature = "bittorrent")]
    bt_registry: Option<Arc<std::sync::RwLock<crate::engine::bittorrent::registry::BtRegistry>>>,
    #[cfg(feature = "bittorrent")]
    lpd_manager: Option<Arc<crate::engine::bittorrent::discovery::lpd::LpdManager>>,
}

impl MagnetDownloadCommand {
    pub fn new(
        gid: GroupId,
        magnet_uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![magnet_uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group(group, output_dir)
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, the internally-created `BtDownloadCommand` is given this
    /// limiter so the resolved torrent's piece writes share the global ceiling.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
    }

    pub fn set_outbound_network_policy(
        &mut self,
        policy: Arc<crate::network::OutboundNetworkPolicy>,
    ) {
        self.outbound_network_policy = policy;
    }

    /// Create a magnet download command that reuses an externally-managed
    /// `RequestGroup` (e.g. from the engine's promotion flow).
    ///
    /// The first URI in the group is treated as the magnet link. Output
    /// directory falls back to the group's `DownloadOptions` when not
    /// explicitly overridden. The group's existing GID and progress counters
    /// are reused.
    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let (magnet_uri, options) = {
            let g = group.recover();
            let uri = g.uris().first().cloned().ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config(
                    "RequestGroup has no URIs for magnet download".into(),
                ))
            })?;
            let opts = g.options_arc();
            (uri, opts)
        };

        let _ml = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&magnet_uri)
            .map_err(|e| Aria2Error::MagnetParse(format!("Invalid magnet link: {}", e)))?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = options
            .out
            .as_deref()
            .map(validate_magnet_output_name)
            .transpose()?
            .or_else(|| {
                _ml.display_name
                    .as_deref()
                    .and_then(sanitize_magnet_display_name)
            })
            .unwrap_or_else(|| "magnet_download".to_string());
        let path = std::path::PathBuf::from(&dir).join(&filename);

        info!(
            "MagnetDownloadCommand created (shared group): {} -> {} (hash={})",
            filename,
            path.display(),
            _ml.info_hash_hex()
        );

        Ok(Self {
            group,
            magnet_uri: magnet_uri.to_string(),
            output_path: path,
            started: false,
            completed_bytes: 0,
            metadata_complete: false,
            dht_engines: DhtEngineSet::default(),
            global_limiter: None,
            outbound_network_policy: Arc::new(crate::network::OutboundNetworkPolicy::direct()),
            #[cfg(feature = "bittorrent")]
            public_tracker_catalog: None,
            #[cfg(feature = "bittorrent")]
            bt_listener: None,
            #[cfg(feature = "bittorrent")]
            bt_registry: None,
            #[cfg(feature = "bittorrent")]
            lpd_manager: None,
        })
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_public_tracker_catalog(
        &mut self,
        catalog: Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>,
    ) {
        self.public_tracker_catalog = Some(catalog);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_listener(
        &mut self,
        listener: Arc<crate::engine::bittorrent::peer::listener::BtPeerListenerManager>,
    ) {
        self.bt_listener = Some(listener);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_registry(
        &mut self,
        registry: Arc<std::sync::RwLock<crate::engine::bittorrent::registry::BtRegistry>>,
    ) {
        self.bt_registry = Some(registry);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_lpd_manager(
        &mut self,
        manager: Arc<crate::engine::bittorrent::discovery::lpd::LpdManager>,
    ) {
        self.lpd_manager = Some(manager);
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    fn clear_registered_dht_engines(&self) {
        if let Some(registry) = self.bt_registry.as_ref()
            && let Ok(mut registry) = registry.write()
        {
            let gid = self.group.recover().gid().value();
            for engine in self.dht_engines.iter() {
                registry.clear_dht_engine_for_gid_if(gid, engine);
            }
        }
    }

    async fn ensure_dht_engines(&mut self, options: &DownloadOptions) -> Result<()> {
        let gid = self.group.recover().gid().value();
        for (use_ipv6, enabled) in [(false, options.enable_dht), (true, options.enable_dht6)] {
            let family_present = if use_ipv6 {
                self.dht_engines.ipv6().is_some()
            } else {
                self.dht_engines.ipv4().is_some()
            };
            if !enabled || family_present {
                continue;
            }

            if self
                .dht_engines
                .attach_global_if_present(self.bt_registry.as_ref(), gid, use_ipv6)
            {
                continue;
            }

            let config =
                crate::engine::bittorrent::dht::config::build_dht_engine_config_for_family(
                    options,
                    &self.outbound_network_policy,
                    use_ipv6,
                )
                .await?;
            match self
                .dht_engines
                .start_or_join(self.bt_registry.as_ref(), gid, config)
                .await
            {
                Ok(()) => info!(ipv6 = use_ipv6, "Magnet: DHT family engine ready"),
                Err(error) => {
                    warn!(ipv6 = use_ipv6, %error, "Magnet: DHT family engine start failed")
                }
            }
        }

        Ok(())
    }

    async fn handoff_dht_engine_to_bt(
        &mut self,
        bt_command: &mut crate::engine::bittorrent::download::command::BtDownloadCommand,
    ) {
        let options = bt_command.group.recover().options().clone();
        let dht_enabled = !bt_command.is_private && (options.enable_dht || options.enable_dht6);
        if !dht_enabled {
            return;
        }
        bt_command.dht_engines = std::mem::take(&mut self.dht_engines);
    }

    async fn discover_magnet_peers(
        &self,
        engines: &DhtEngineSet,
        info_hash: &[u8; 20],
    ) -> Vec<std::net::SocketAddr> {
        const MAX_ATTEMPTS: usize = 4;
        const RETRY_DELAYS: [Duration; 3] = [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ];

        if engines.is_empty() {
            return Vec::new();
        }
        let readiness = futures::future::join_all(engines.iter().map(|engine| async move {
            (
                engine.local_addr(),
                engine.wait_until_ready(Duration::from_secs(5)).await,
            )
        }))
        .await;
        for (address, result) in readiness {
            if let Err(error) = result {
                warn!(%address, %error, "Magnet: DHT family engine did not become ready before lookup");
            }
        }

        for attempt in 0..MAX_ATTEMPTS {
            match engines.find_peers(info_hash).await {
                Ok(result) if !result.peers.is_empty() => {
                    info!(
                        "Magnet: DHT discovered {} peers (contacted {} nodes, attempt {})",
                        result.peers.len(),
                        result.nodes_contacted,
                        attempt + 1
                    );
                    return result.peers;
                }
                Ok(result) => {
                    warn!(
                        "Magnet: DHT lookup found no peers (contacted {} nodes, attempt {}/{})",
                        result.nodes_contacted,
                        attempt + 1,
                        MAX_ATTEMPTS
                    );
                }
                Err(error) => {
                    warn!(
                        "Magnet: DHT find_peers failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_ATTEMPTS,
                        error
                    );
                }
            }

            if let Some(delay) = RETRY_DELAYS.get(attempt) {
                tokio::time::sleep(*delay).await;
            }
        }

        Vec::new()
    }

    async fn discover_magnet_tracker_peers(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        options: &DownloadOptions,
    ) -> Vec<SocketAddr> {
        let tracker_urls = metadata_tracker_urls(magnet, options);
        if tracker_urls.is_empty() {
            return Vec::new();
        }

        let peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id_with_prefix(
            &options.peer_id_prefix,
        );
        let tracker_tls = ClientTlsConfig::from_download_options(options);
        let max_tracker_timeout = Duration::from_secs(options.bt_tracker_timeout.max(1));
        let connect_timeout = Duration::from_secs(options.bt_tracker_connect_timeout.max(1));
        let announce_port = options
            .listen_port
            .as_deref()
            .and_then(|value| value.split(['-', ',']).next())
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        let mut discovered_peers = Vec::new();

        for tracker_url in tracker_urls.into_iter().take(MAX_MAGNET_TRACKERS_TO_TRY) {
            let tracker_scheme = reqwest::Url::parse(&tracker_url)
                .ok()
                .map(|url| url.scheme().to_owned());
            if matches!(tracker_scheme.as_deref(), Some("ws" | "wss")) {
                match crate::engine::bittorrent::tracker::websocket::announce_with_policy(
                    &tracker_url,
                    crate::engine::bittorrent::tracker::websocket::AnnounceRequest {
                        info_hash: &magnet.info_hash,
                        peer_id: &peer_id,
                        downloaded: 0,
                        left: magnet.exact_length.unwrap_or(0),
                        uploaded: 0,
                        numwant: 50,
                        port: announce_port,
                        event: crate::engine::bittorrent::tracker::communication::AnnounceEvent::Started,
                        options,
                    },
                    &self.outbound_network_policy,
                )
                .await
                {
                    Ok(response) if !response.peers.is_empty() => {
                        info!(
                            tracker = %tracker_url,
                            peers = response.peers.len(),
                            "WebSocket tracker announce returned peers"
                        );
                        append_unique_tracker_peers(&mut discovered_peers, response.peers);
                    }
                    Ok(_) => {
                        warn!(tracker = %tracker_url, "WebSocket tracker returned no TCP peers");
                    }
                    Err(error) => {
                        warn!(tracker = %tracker_url, %error, "WebSocket tracker announce failed");
                    }
                }
                if discovered_peers.len() >= MAX_MAGNET_TRACKER_PEERS {
                    break;
                }
                continue;
            }

            let tracker_tiers = vec![vec![tracker_url.clone()]];
            let mut announcer = TrackerAnnouncer::new(&tracker_tiers, &None);
            announcer.set_outbound_network_policy(Arc::clone(&self.outbound_network_policy));
            announcer.set_http_tls_config(tracker_tls.clone());
            announcer.set_timeouts(max_tracker_timeout, connect_timeout);
            announcer.set_tcp_port(announce_port);

            let result = announcer
                .announce(
                    &magnet.info_hash,
                    &peer_id,
                    0,
                    magnet.exact_length.unwrap_or(0),
                    0,
                )
                .await;

            let Some(result) = result else {
                warn!(tracker = %tracker_url, "Magnet tracker announce failed");
                continue;
            };

            let peers: Vec<SocketAddr> = result
                .peers
                .iter()
                .filter_map(|(ip, port)| tracker_peer_socket_addr(ip, *port))
                .collect();
            info!(
                tracker = %tracker_url,
                peers = peers.len(),
                "Magnet tracker announce returned peers"
            );
            append_unique_tracker_peers(&mut discovered_peers, peers);

            if discovered_peers.len() >= MAX_MAGNET_TRACKER_PEERS {
                break;
            }
        }

        if !discovered_peers.is_empty() {
            info!(
                peers = discovered_peers.len(),
                "Magnet tracker peer discovery completed"
            );
        }
        discovered_peers
    }

    async fn shutdown_dht_engine(&mut self) {
        self.clear_registered_dht_engines();
        for engine in std::mem::take(&mut self.dht_engines).into_vec() {
            let is_shared = self
                .bt_registry
                .as_ref()
                .and_then(|registry| registry.read().ok())
                .is_some_and(|registry| registry.is_global_dht_engine(&engine));
            if !is_shared {
                engine.shutdown_async().await;
            }
        }
    }

    fn saved_metadata_path(&self, info_hash: &[u8; 20]) -> std::path::PathBuf {
        self.output_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}.torrent", hex::encode(info_hash)))
    }

    fn saved_metadata_path_for_magnet(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    ) -> std::path::PathBuf {
        let name = magnet
            .info_hash_v2
            .map(hex::encode)
            .unwrap_or_else(|| hex::encode(magnet.info_hash));
        self.output_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}.torrent", name))
    }

    fn load_saved_metadata(&self, info_hash: &[u8; 20]) -> Option<Vec<u8>> {
        let path = self.saved_metadata_path(info_hash);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(path = %path.display(), %error, "Failed to read saved BitTorrent metadata");
                return None;
            }
        };

        match aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&data) {
            Ok(meta) if meta.info_hash.bytes == *info_hash => {
                info!(path = %path.display(), "Loaded BitTorrent metadata from saved torrent file");
                Some(data)
            }
            Ok(meta) => {
                warn!(
                    path = %path.display(),
                    actual_info_hash = %meta.info_hash.as_hex(),
                    expected_info_hash = %hex::encode(info_hash),
                    "Ignoring saved BitTorrent metadata with unexpected info-hash"
                );
                None
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "Ignoring invalid saved BitTorrent metadata");
                None
            }
        }
    }

    fn load_saved_metadata_for_magnet(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    ) -> Option<Vec<u8>> {
        if magnet.info_hash_v2.is_none() {
            return self.load_saved_metadata(&magnet.info_hash);
        }
        let path = self.saved_metadata_path_for_magnet(magnet);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(path = %path.display(), %error, "Failed to read saved BitTorrent metadata");
                return None;
            }
        };
        match Self::metadata_matches_magnet(magnet, &data) {
            Ok(()) => {
                info!(path = %path.display(), "Loaded BitTorrent metadata from saved torrent file");
                Some(data)
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "Ignoring saved BitTorrent metadata with unexpected info-hash");
                None
            }
        }
    }

    fn metadata_matches_magnet(
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        torrent_bytes: &[u8],
    ) -> std::result::Result<(), String> {
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)?;
        if let Some(expected_v1) = magnet.info_hash_v1
            && meta.info_hash.bytes != expected_v1
        {
            return Err(format!(
                "v1 info-hash mismatch: expected {}, got {}",
                hex::encode(expected_v1),
                meta.info_hash.as_hex()
            ));
        }
        if let Some(expected_v2) = magnet.info_hash_v2
            && meta.info_hash_v2 != Some(expected_v2)
        {
            return Err(format!(
                "v2 info-hash mismatch: expected {}, got {}",
                hex::encode(expected_v2),
                meta.info_hash_v2
                    .map(hex::encode)
                    .unwrap_or_else(|| "absent".into())
            ));
        }
        Ok(())
    }

    /// Convert BEP 9's raw `info` dictionary into the complete metainfo
    /// document consumed by the torrent parser and saved-metadata path.
    ///
    /// `ut_metadata` transfers only the bencoded `info` dictionary.  Exact
    /// sources and previously saved metadata already contain a torrent root,
    /// so those inputs are kept unchanged.
    fn normalize_magnet_metadata(
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        metadata: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let (value, consumed) = BencodeValue::decode(metadata)?;
        if consumed != metadata.len() {
            return Err("BEP 9 metadata contains trailing bytes".to_string());
        }

        let BencodeValue::Dict(info_dict) = value else {
            return Err("BEP 9 metadata is not a dictionary".to_string());
        };

        // Keep complete .torrent documents from xs and the saved-metadata
        // path idempotent. A BEP 9 info dictionary is itself also a dict, so
        // the presence of the root-level `info` key is the discriminator.
        if info_dict
            .get(b"info".as_slice())
            .is_some_and(|value| matches!(value, BencodeValue::Dict(_)))
        {
            return Ok(metadata.to_vec());
        }

        let announce = magnet
            .trackers
            .first()
            .map_or_else(Vec::new, |url| url.as_bytes().to_vec());
        let announce_list = (!magnet.trackers.is_empty()).then(|| {
            BencodeValue::List(
                magnet
                    .trackers
                    .iter()
                    .map(|url| {
                        BencodeValue::List(vec![BencodeValue::Bytes(url.as_bytes().to_vec())])
                    })
                    .collect(),
            )
        });

        let mut root = BTreeMap::new();
        root.insert(b"announce".to_vec(), BencodeValue::Bytes(announce));
        if let Some(announce_list) = announce_list {
            root.insert(b"announce-list".to_vec(), announce_list);
        }
        root.insert(b"info".to_vec(), BencodeValue::Dict(info_dict));
        Ok(BencodeValue::Dict(root).encode())
    }

    /// Fetch torrent metadata from BEP 9's `xs` exact-source parameter.
    ///
    /// An exact source is a complete `.torrent` file, so it is a cheaper and
    /// more deterministic metadata path than starting DHT and waiting for a
    /// metadata-capable peer. HTTP(S) sources use the same proxy, TLS, and
    /// authentication options as ordinary HTTP downloads. Local `file://`
    /// sources are also accepted and still go through the info-hash check.
    async fn fetch_magnet_exact_source(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        options: &DownloadOptions,
    ) -> Option<Vec<u8>> {
        let sources = magnet.exact_sources.iter().filter_map(|source| {
            let url = reqwest::Url::parse(source).ok()?;
            matches!(url.scheme(), "file" | "http" | "https").then_some((source, url))
        });
        let sources: Vec<_> = sources.collect();
        if sources.is_empty() {
            return None;
        }

        let request_timeout = options.timeout.unwrap_or(options.bt_tracker_timeout).max(1);
        let connect_timeout = options
            .connect_timeout
            .unwrap_or(options.bt_tracker_connect_timeout)
            .max(1);

        let client = if sources
            .iter()
            .any(|(_, url)| matches!(url.scheme(), "http" | "https"))
        {
            match Self::build_magnet_exact_source_client(
                options,
                request_timeout,
                connect_timeout,
                &self.outbound_network_policy,
            ) {
                Ok(client) => Some(client),
                Err(error) => {
                    warn!(%error, "Magnet exact-source HTTP client creation failed");
                    None
                }
            }
        } else {
            None
        };

        for (source, url) in sources {
            let body = if url.scheme() == "file" {
                let path = match url.to_file_path() {
                    Ok(path) => path,
                    Err(()) => {
                        warn!(source = %source, "Magnet exact-source file URL has no local path");
                        continue;
                    }
                };
                match tokio::fs::read(&path).await {
                    Ok(body) => body,
                    Err(error) => {
                        warn!(source = %source, path = %path.display(), %error, "Magnet exact-source file read failed");
                        continue;
                    }
                }
            } else {
                let Some(client) = client.as_ref() else {
                    continue;
                };
                let response = match Self::request_magnet_exact_source(client, &url, options).await
                {
                    Ok(response) => response,
                    Err(error) => {
                        warn!(source = %source, %error, "Magnet exact-source request failed");
                        continue;
                    }
                };
                if !response.status().is_success() {
                    warn!(
                        source = %source,
                        status = %response.status(),
                        "Magnet exact-source request returned an error status"
                    );
                    continue;
                }

                match response.bytes().await {
                    Ok(body) => body.to_vec(),
                    Err(error) => {
                        warn!(source = %source, %error, "Magnet exact-source response read failed");
                        continue;
                    }
                }
            };
            match Self::metadata_matches_magnet(magnet, &body) {
                Ok(()) => {
                    info!(source = %source, bytes = body.len(), "Loaded magnet metadata from exact source");
                    return Some(body);
                }
                Err(error) => {
                    warn!(source = %source, %error, "Ignoring exact-source metadata with mismatched info-hash");
                }
            }
        }

        None
    }

    fn build_magnet_exact_source_client(
        options: &DownloadOptions,
        request_timeout: u64,
        connect_timeout: u64,
        policy: &crate::network::OutboundNetworkPolicy,
    ) -> std::result::Result<reqwest::Client, String> {
        crate::http::client_pool::ensure_rustls_provider();
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(request_timeout))
            .connect_timeout(Duration::from_secs(connect_timeout))
            .gzip(false)
            .user_agent(crate::constants::USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(5));
        if let Some(address) = policy.addresses().into_iter().next() {
            builder = builder.local_address(address);
        }
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
        if let Some(proxy) = options
            .all_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
            && matches!(
                ProxyUrl::parse(proxy).map(|parsed| parsed.protocol),
                Ok(crate::http::socks_connector::ProxyProtocol::Http)
                    | Ok(crate::http::socks_connector::ProxyProtocol::Https)
            )
        {
            builder = add_reqwest_proxy(
                builder,
                ProxyTarget::All,
                proxy,
                options.proxy_credentials_for_scheme("all"),
                no_proxy,
            );
        }

        let tls = ClientTlsConfig::from_download_options(options);
        let builder = crate::http::client_identity::apply(builder, &tls)
            .map_err(|error| error.to_string())?;
        builder
            .build()
            .map_err(|error| format!("failed to build exact-source HTTP client: {error}"))
    }

    async fn request_magnet_exact_source(
        client: &reqwest::Client,
        url: &reqwest::Url,
        options: &DownloadOptions,
    ) -> std::result::Result<reqwest::Response, String> {
        let auth_context = crate::engine::http::auth::from_options(options, url.scheme());
        let (mut auth_factory, auth_options) = (auth_context.factory, auth_context.options);
        let mut request = client.get(url.clone());
        if let Some(authorization) = auth_factory.resolve_basic_authorization(url, &auth_options) {
            request = request.header(reqwest::header::AUTHORIZATION, authorization);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED || !options.http_auth_challenge {
            return Ok(response);
        }

        let challenge_header = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let scheme = challenge_header
            .as_deref()
            .and_then(crate::http::AuthScheme::from_header)
            .unwrap_or(crate::http::AuthScheme::Basic);
        let challenge = crate::http::HttpAuthChallenge {
            scheme: scheme.clone(),
            realm: challenge_header
                .as_deref()
                .map(crate::http::HttpSkipResponseHandler::extract_realm)
                .unwrap_or_default(),
            is_proxy: false,
            digest_challenge: if scheme == crate::http::AuthScheme::Digest {
                challenge_header.as_deref().and_then(|header| {
                    crate::http::digest_auth::DigestAuthChallenge::parse(header).ok()
                })
            } else {
                None
            },
        };
        let auth_result = crate::http::handle_auth_challenge(
            &challenge,
            &mut auth_factory,
            url,
            &auth_options,
            crate::http::request::HttpMethod::Get,
            false,
            1,
        );
        let crate::http::AuthChallengeResult::RetryWithAuth {
            authorization_header,
            is_proxy: false,
        } = auth_result
        else {
            return Ok(response);
        };

        client
            .get(url.clone())
            .header(reqwest::header::AUTHORIZATION, authorization_header)
            .send()
            .await
            .map_err(|error| error.to_string())
    }

    /// Add web seeds from the magnet URI to the resolved torrent metadata.
    ///
    /// `ws` is a root-level torrent field, so adding it does not change the
    /// info dictionary or its v1/v2 info-hash. Keeping it in the resolved
    /// metadata lets the existing BitTorrent context and web-seed manager
    /// consume it without introducing a second magnet-only configuration
    /// path.
    fn merge_magnet_web_seeds(
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        torrent_bytes: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

        if magnet.ws.is_empty() {
            return Ok(torrent_bytes.to_vec());
        }

        let (mut root, consumed) = BencodeValue::decode(torrent_bytes)?;
        if consumed != torrent_bytes.len() {
            return Err("Torrent metadata contains trailing bytes".to_string());
        }
        let dict = match &mut root {
            BencodeValue::Dict(dict) => dict,
            _ => return Err("Torrent metadata root is not a dictionary".to_string()),
        };

        let mut web_seeds = Vec::new();
        if let Some(existing) = dict.remove(b"url-list".as_slice()) {
            match existing {
                BencodeValue::Bytes(url) => web_seeds.push(url),
                BencodeValue::List(urls) => {
                    web_seeds.extend(urls.into_iter().filter_map(|url| match url {
                        BencodeValue::Bytes(url) => Some(url),
                        _ => None,
                    }));
                }
                _ => {}
            }
        }
        for url in &magnet.ws {
            let url = url.as_bytes().to_vec();
            if !web_seeds.iter().any(|existing| existing == &url) {
                web_seeds.push(url);
            }
        }

        if web_seeds.len() == 1 {
            dict.insert(
                b"url-list".to_vec(),
                BencodeValue::Bytes(web_seeds[0].clone()),
            );
        } else {
            dict.insert(
                b"url-list".to_vec(),
                BencodeValue::List(web_seeds.into_iter().map(BencodeValue::Bytes).collect()),
            );
        }
        Ok(root.encode())
    }

    fn save_metadata_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<bool> {
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(error),
        };

        if let Err(error) = file.write_all(data).and_then(|_| file.flush()) {
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(true)
    }

    async fn fetch_magnet_metadata(
        &mut self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    ) -> Result<Vec<u8>> {
        let (enable_dht, options) = {
            let group = self.group.recover();
            (group.options().enable_dht, group.options().clone())
        };

        // `xs` points to complete torrent metadata. Try it before starting
        // DHT so a magnet with a working exact source does not pay the DHT
        // bootstrap/lookup cost at all.
        if let Some(metadata) = self.fetch_magnet_exact_source(magnet, &options).await {
            return Ok(metadata);
        }

        let outbound_network_policy = Arc::clone(&self.outbound_network_policy);
        let metadata_session = |max_peers_to_try| {
            MetadataExchangeSession::new(MetadataExchangeConfig {
                max_peers_to_try,
                connect_timeout: Duration::from_secs(15),
                request_timeout: Duration::from_secs(10),
                piece_size: 16 * 1024,
                ..MetadataExchangeConfig::default()
            })
            .with_outbound_network_policy(Arc::clone(&outbound_network_policy))
        };

        // Magnet links commonly carry tracker URLs, and tracker discovery is
        // available even when DHT bootstrap is blocked by NAT or a firewall.
        // Try those peers first so a working tracker does not wait for a DHT
        // lookup that may never produce a result.
        let tracker_peers = self.discover_magnet_tracker_peers(magnet, &options).await;
        let mut last_error = None;
        if !tracker_peers.is_empty() {
            match metadata_session(tracker_peers.len().min(MAX_MAGNET_METADATA_PEERS_TO_TRY))
                .fetch_metadata(&magnet.info_hash, &tracker_peers)
                .await
            {
                Ok(metadata) => return Ok(metadata),
                Err(error) => {
                    warn!(
                        "Magnet: metadata fetch from tracker peers failed: {}",
                        error
                    );
                    last_error = Some(error.to_string());
                }
            }
        }

        if enable_dht || options.enable_dht6 {
            self.ensure_dht_engines(&options).await?;
        }
        let dht_peers = if !self.dht_engines.is_empty() {
            self.discover_magnet_peers(&self.dht_engines, &magnet.info_hash)
                .await
        } else {
            warn!("Magnet: DHT disabled and tracker discovery returned no usable peers");
            Vec::new()
        };

        if !dht_peers.is_empty() {
            match metadata_session(dht_peers.len().min(MAX_MAGNET_METADATA_PEERS_TO_TRY))
                .fetch_metadata(&magnet.info_hash, &dht_peers)
                .await
            {
                Ok(metadata) => return Ok(metadata),
                Err(error) => last_error = Some(error.to_string()),
            }
        }

        let message = last_error.map_or_else(
            || "No peers found via trackers or DHT".to_string(),
            |error| format!("Metadata fetch failed after tracker and DHT discovery: {error}"),
        );
        Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure { message },
        ))
    }

    /// BEP 0027 (Private Torrent) enforcement after metadata exchange.
    ///
    /// For magnet links, a DHT engine may be borrowed or started BEFORE metadata arrives
    /// because DHT-based peer discovery is required to find peers that can
    /// serve the metadata via BEP 0010 (Extension for Peers to Send Metadata
    /// File). Once the metadata has been fetched, if the torrent's `private`
    /// flag is set, this torrent must stop using DHT to comply with BEP 0027,
    /// which forbids DHT, PEX, and LPD for private torrents. A process-global
    /// engine borrowed by this command remains alive for other torrents.
    ///
    /// This method parses the fetched `torrent_bytes`, checks `is_private()`,
    /// and if true, releases the magnet's DHT handle. An exclusively owned
    /// temporary engine is shut down; a shared process engine is left running.
    /// The downstream `BtDownloadCommand` then enforces BEP 0027 and will not
    /// use or start DHT for this torrent.
    ///
    /// Extracted as a standalone async method so the policy logic can be
    /// unit tested without mocking the BEP 0010 metadata exchange network I/O.
    async fn enforce_bep0027_after_metadata(&mut self, torrent_bytes: &[u8]) -> Result<()> {
        let is_private =
            aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!(
                        "Fetched metadata parse failed: {}",
                        e
                    )))
                })?
                .is_private();

        if is_private {
            info!("Private torrent detected after metadata exchange: shutting down DHT (BEP 0027)");
            self.shutdown_dht_engine().await;
            // shutdown_dht_engine empties this family-scoped set, so the
            // downstream BtDownloadCommand cannot accidentally reuse it.
        }

        Ok(())
    }
}

fn validate_magnet_output_name(name: &str) -> Result<String> {
    let path = std::path::Path::new(name);
    let is_single_component = path.components().count() == 1;
    if name.is_empty() || !is_single_component || name == "." || name == ".." {
        return Err(Aria2Error::Fatal(FatalError::Config(format!(
            "magnet output name must be a file name, got '{name}'"
        ))));
    }
    Ok(name.to_owned())
}

fn sanitize_magnet_display_name(name: &str) -> Option<String> {
    validate_magnet_output_name(name).ok()
}

#[async_trait]
impl Command for MagnetDownloadCommand {
    async fn shutdown(&mut self) {
        self.shutdown_dht_engine().await;
    }

    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        let ml = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&self.magnet_uri)
            .map_err(|e| Aria2Error::MagnetParse(format!("Magnet parse error: {}", e)))?;

        info!(
            "Magnet download: hash={}, name={:?}",
            ml.info_hash_hex(),
            ml.display_name
        );

        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        let (load_saved_metadata, save_metadata, metadata_only) = {
            let group = self.group.recover();
            (
                group.options().bt_load_saved_metadata,
                group.options().bt_save_metadata,
                group.options().bt_metadata_only,
            )
        };

        let torrent_bytes = if load_saved_metadata {
            if let Some(data) = self.load_saved_metadata_for_magnet(&ml) {
                data
            } else {
                self.fetch_magnet_metadata(&ml).await?
            }
        } else {
            self.fetch_magnet_metadata(&ml).await?
        };

        let torrent_bytes =
            Self::normalize_magnet_metadata(&ml, &torrent_bytes).map_err(|error| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "Fetched magnet metadata is invalid: {error}"
                )))
            })?;

        let torrent_bytes = Self::merge_magnet_web_seeds(&ml, &torrent_bytes).map_err(|error| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Fetched metadata could not include magnet web seeds: {error}"
            )))
        })?;

        Self::metadata_matches_magnet(&ml, &torrent_bytes).map_err(|error| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Fetched metadata does not match magnet: {}",
                error
            )))
        })?;

        info!("Fetched torrent metadata: {} bytes", torrent_bytes.len());

        // BEP 0027 (Private Torrent): DHT was started before metadata exchange
        // for peer discovery. Now that the metadata is available, parse it and
        // shut down DHT if the torrent is private. The downstream
        // BtDownloadCommand (created from the same bytes) will also enforce
        // BEP 0027 by refusing to start its own DHT when is_private is set.
        self.enforce_bep0027_after_metadata(&torrent_bytes).await?;

        if save_metadata {
            let path = self.saved_metadata_path_for_magnet(&ml);
            match Self::save_metadata_file(&path, &torrent_bytes) {
                Ok(true) => info!(path = %path.display(), "Saved BitTorrent metadata"),
                Ok(false) => {
                    info!(path = %path.display(), "BitTorrent metadata file already exists; keeping it")
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "Failed to save BitTorrent metadata")
                }
            }
        }

        if metadata_only {
            self.shutdown_dht_engine().await;
            self.group.recover_mut().complete()?;
            self.metadata_complete = true;
            DownloadEventHooks::shared().notify_metadata_resolved(MetadataResolvedEvent::new(
                self.group.recover().gid(),
                Vec::new(),
            ));
            info!("Magnet metadata download complete; payload download skipped");
            return Ok(());
        }

        use crate::engine::bittorrent::download::command::BtDownloadCommand;
        let gid = self.group.recover().gid();
        let mut bt_cmd = BtDownloadCommand::new_with_group_and_mappings_with_policy(
            Arc::clone(&self.group),
            &torrent_bytes,
            self.group.recover().options(),
            self.output_path.parent().and_then(|p| p.to_str()),
            &[],
            &self.outbound_network_policy,
        )?;
        DownloadEventHooks::shared()
            .notify_metadata_resolved(MetadataResolvedEvent::new(gid, vec![gid]));
        if let Some(gl) = self.global_limiter.clone() {
            bt_cmd.set_global_limiter(gl);
        }
        bt_cmd.set_outbound_network_policy(Arc::clone(&self.outbound_network_policy));
        #[cfg(feature = "bittorrent")]
        if let Some(catalog) = self.public_tracker_catalog.clone() {
            bt_cmd.set_public_tracker_catalog(catalog);
        }
        if let Some(listener) = self.bt_listener.clone() {
            bt_cmd.set_bt_listener(listener);
        }
        if let Some(registry) = self.bt_registry.clone() {
            bt_cmd.set_bt_registry(registry);
        }
        if let Some(manager) = self.lpd_manager.clone() {
            bt_cmd.set_lpd_manager(manager);
        }

        self.handoff_dht_engine_to_bt(&mut bt_cmd).await;

        bt_cmd.execute().await?;

        self.shutdown_dht_engine().await;

        self.completed_bytes = self.group.recover().total_length();

        info!("Magnet download complete: {}", self.output_path.display());
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.metadata_complete
            || self.group.recover().status()
                == crate::request::request_group::DownloadStatus::Complete
        {
            CommandStatus::Completed
        } else if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
    }

    fn request_group(
        &self,
    ) -> Option<std::sync::Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>>
    {
        Some(std::sync::Arc::clone(&self.group))
    }

    fn timeout(&self) -> Option<Duration> {
        self.group.recover().timeout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bittorrent::download::command_tests::{
        build_private_test_torrent, build_test_torrent,
    };

    #[derive(Default)]
    struct MetadataEventListener {
        events: std::sync::Mutex<Vec<MetadataResolvedEvent>>,
        alive: std::sync::atomic::AtomicBool,
    }

    impl MetadataEventListener {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
                alive: std::sync::atomic::AtomicBool::new(true),
            }
        }
    }

    impl crate::engine::download_event_hooks::DownloadEventListener for MetadataEventListener {
        fn on_download_event(
            &self,
            _event: crate::engine::download_event_hooks::DownloadEvent,
            _gid: &str,
        ) {
        }

        fn on_metadata_resolved(&self, event: &MetadataResolvedEvent) {
            self.events.lock().unwrap().push(event.clone());
        }

        fn is_alive(&self) -> bool {
            self.alive.load(std::sync::atomic::Ordering::Acquire)
        }
    }

    /// Valid 40-char hex info-hash magnet link used by all test cases.
    const TEST_MAGNET_URI: &str =
        "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=test_file";

    fn make_test_command() -> MagnetDownloadCommand {
        MagnetDownloadCommand::new(
            GroupId::new(1),
            TEST_MAGNET_URI,
            &DownloadOptions::default(),
            None,
        )
        .expect("Failed to create test MagnetDownloadCommand")
    }

    fn make_test_command_in_dir(dir: &std::path::Path) -> MagnetDownloadCommand {
        MagnetDownloadCommand::new(
            GroupId::new(2),
            TEST_MAGNET_URI,
            &DownloadOptions::default(),
            dir.to_str(),
        )
        .expect("Failed to create MagnetDownloadCommand in temporary directory")
    }

    #[test]
    fn default_directory_is_valid_for_a_safe_magnet_name() {
        let command = MagnetDownloadCommand::new(
            GroupId::new(4),
            "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=test_file",
            &DownloadOptions::default(),
            Some("."),
        )
        .expect("a directory is not an output file");

        assert_eq!(
            command.output_path,
            std::path::PathBuf::from(".").join("test_file")
        );
    }

    #[test]
    fn magnet_metadata_discovery_uses_embedded_and_configured_trackers() {
        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(
            "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&tr=udp%3A%2F%2Fembedded.test%3A6969&tr=http%3A%2F%2Fexcluded.test%2Fannounce&tr=wss%3A%2F%2Ftracker.example%2Fannounce",
        )
        .expect("test magnet should parse");
        let options = DownloadOptions {
            bt_tracker: Some(vec![
                "https://configured.test/announce".to_string(),
                "udp://embedded.test:6969".to_string(),
            ]),
            bt_exclude_tracker: Some(vec!["http://excluded.test/announce".to_string()]),
            ..DownloadOptions::default()
        };

        assert_eq!(
            metadata_tracker_urls(&magnet, &options),
            vec![
                "udp://embedded.test:6969".to_string(),
                "wss://tracker.example/announce".to_string(),
                "https://configured.test/announce".to_string(),
            ]
        );
    }

    #[test]
    fn tracker_peer_addresses_accept_ipv4_and_ipv6() {
        assert_eq!(
            tracker_peer_socket_addr("192.0.2.10", 6881),
            Some("192.0.2.10:6881".parse().unwrap())
        );
        assert_eq!(
            tracker_peer_socket_addr("2001:db8::10", 6881),
            Some("[2001:db8::10]:6881".parse().unwrap())
        );
        assert!(tracker_peer_socket_addr("not-an-ip", 6881).is_none());
    }

    #[test]
    fn tracker_peer_collection_deduplicates_and_bounds_results() {
        let mut discovered = Vec::new();
        let peers = (0..(MAX_MAGNET_TRACKER_PEERS + 5))
            .map(|offset| SocketAddr::from(([192, 0, 2, 1], 10_000 + offset as u16)))
            .collect();

        append_unique_tracker_peers(&mut discovered, peers);
        assert_eq!(discovered.len(), MAX_MAGNET_TRACKER_PEERS);

        let duplicate = discovered[0];
        append_unique_tracker_peers(&mut discovered, vec![duplicate]);
        assert_eq!(discovered.len(), MAX_MAGNET_TRACKER_PEERS);
    }

    #[test]
    fn normalize_bep9_info_metadata_builds_a_complete_torrent() {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

        let torrent = build_test_torrent();
        let (root, consumed) = BencodeValue::decode(&torrent).expect("decode test torrent");
        assert_eq!(consumed, torrent.len());
        let info = root.dict_get(b"info").expect("test torrent info dict");
        let info_bytes = info.encode();
        let expected_hash =
            aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
                .unwrap()
                .info_hash;
        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{}&tr=http%3A%2F%2Ftracker.test%2Fannounce",
            expected_hash.as_hex()
        ))
        .expect("test magnet should parse");

        let normalized = MagnetDownloadCommand::normalize_magnet_metadata(&magnet, &info_bytes)
            .expect("BEP 9 info metadata should be wrapped");
        let parsed = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&normalized)
            .expect("normalized metadata should parse as a torrent");

        assert_eq!(parsed.info_hash.bytes, magnet.info_hash);
        assert_eq!(parsed.announce, "http://tracker.test/announce");
        assert_eq!(parsed.info_hash.bytes, expected_hash.bytes);
    }

    #[test]
    fn normalize_magnet_metadata_keeps_complete_torrent_bytes() {
        let torrent = build_test_torrent();
        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(
            "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678",
        )
        .expect("test magnet should parse");

        let normalized = MagnetDownloadCommand::normalize_magnet_metadata(&magnet, &torrent)
            .expect("complete torrent metadata should remain valid");
        assert_eq!(normalized, torrent);
    }

    #[tokio::test]
    async fn magnet_exact_source_returns_matching_torrent_without_dht() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let torrent = build_test_torrent();
        let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent should parse")
            .info_hash
            .bytes;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind exact-source test server");
        let address = listener.local_addr().expect("read test server address");
        let server_torrent = torrent.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept exact-source request");
            let mut request = [0u8; 4096];
            let _request_len = socket
                .read(&mut request)
                .await
                .expect("read exact-source request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_torrent.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write exact-source headers");
            socket
                .write_all(&server_torrent)
                .await
                .expect("write exact-source torrent");
        });

        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{}&xs=http://{}/metadata.torrent",
            hex::encode(info_hash),
            address
        ))
        .expect("exact-source magnet should parse");
        assert_eq!(magnet.exact_sources.len(), 1);
        MagnetDownloadCommand::metadata_matches_magnet(&magnet, &torrent)
            .expect("test magnet hash should match test torrent");
        let command = make_test_command();
        let metadata = command
            .fetch_magnet_exact_source(&magnet, &DownloadOptions::default())
            .await
            .expect("exact source should return metadata");
        server.await.expect("exact-source server should finish");

        assert_eq!(metadata, torrent);
        assert!(command.dht_engines.is_empty());
    }

    #[tokio::test]
    async fn magnet_exact_source_reads_file_url_without_dht() {
        let torrent = build_test_torrent();
        let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent should parse")
            .info_hash
            .bytes;
        let temp_dir = tempfile::tempdir().expect("temporary exact-source directory");
        let path = temp_dir.path().join("metadata.torrent");
        std::fs::write(&path, &torrent).expect("write exact-source torrent");
        let source =
            url::Url::from_file_path(&path).expect("temporary path should convert to a file URL");
        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{}&xs={source}",
            hex::encode(info_hash)
        ))
        .expect("file exact-source magnet should parse");

        let command = make_test_command();
        let metadata = command
            .fetch_magnet_exact_source(&magnet, &DownloadOptions::default())
            .await
            .expect("file exact source should return metadata");

        assert_eq!(metadata, torrent);
        assert!(command.dht_engines.is_empty());
    }

    #[tokio::test]
    async fn magnet_exact_source_retries_http_basic_auth_challenge() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let torrent = build_test_torrent();
        let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent should parse")
            .info_hash
            .bytes;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind auth exact-source test server");
        let address = listener.local_addr().expect("read auth server address");
        let server_torrent = torrent.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept auth request");
                let mut request = [0u8; 4096];
                let length = socket.read(&mut request).await.expect("read auth request");
                let request = String::from_utf8_lossy(&request[..length]);
                if attempt == 0 {
                    assert!(!request.to_ascii_lowercase().contains("authorization:"));
                    socket
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=exact\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth challenge");
                } else {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("authorization: basic dxnlcjpwyxnz")
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        server_torrent.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.write_all(&server_torrent).await.unwrap();
                }
            }
        });

        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{}&xs=http://{address}/metadata.torrent",
            hex::encode(info_hash)
        ))
        .expect("auth exact-source magnet should parse");
        let options = DownloadOptions {
            http_auth_challenge: true,
            http_user: Some("user".to_string()),
            http_passwd: Some("pass".to_string()),
            ..DownloadOptions::default()
        };
        let metadata = make_test_command()
            .fetch_magnet_exact_source(&magnet, &options)
            .await
            .expect("authenticated exact source should return metadata");
        server
            .await
            .expect("auth exact-source server should finish");
        assert_eq!(metadata, torrent);
    }

    #[tokio::test]
    async fn magnet_exact_source_uses_authenticated_http_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let torrent = build_test_torrent();
        let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent should parse")
            .info_hash
            .bytes;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy exact-source test server");
        let address = listener.local_addr().expect("read proxy server address");
        let server_torrent = torrent.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept proxy request");
            let mut request = [0u8; 8192];
            let length = socket.read(&mut request).await.expect("read proxy request");
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with("GET http://exact-source.invalid/metadata.torrent"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("proxy-authorization: basic dxnlcjpwyxnz")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_torrent.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(&server_torrent).await.unwrap();
        });

        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{}&xs=http://exact-source.invalid/metadata.torrent",
            hex::encode(info_hash)
        ))
        .expect("proxy exact-source magnet should parse");
        let options = DownloadOptions {
            http_proxy: Some(format!("http://{address}")),
            http_proxy_user: Some("user".to_string()),
            http_proxy_passwd: Some("pass".to_string()),
            no_proxy: Some(String::new()),
            ..DownloadOptions::default()
        };
        let metadata = make_test_command()
            .fetch_magnet_exact_source(&magnet, &options)
            .await
            .expect("proxied exact source should return metadata");
        server
            .await
            .expect("proxy exact-source server should finish");
        assert_eq!(metadata, torrent);
    }

    #[test]
    fn magnet_web_seeds_are_added_without_changing_info_hash() {
        let torrent = build_test_torrent();
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent should parse");
        let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{}&ws=https%3A%2F%2Fseed.example%2Ffiles%2F&ws=https%3A%2F%2Fseed.example%2Fmirror%2F",
            meta.info_hash.as_hex()
        ))
        .expect("web-seed magnet should parse");

        let merged = MagnetDownloadCommand::merge_magnet_web_seeds(&magnet, &torrent)
            .expect("web seeds should be merged into torrent metadata");
        let merged_meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&merged)
            .expect("merged torrent should parse");

        assert_eq!(merged_meta.info_hash, meta.info_hash);
        assert_eq!(
            merged_meta.web_seeds,
            vec![
                "https://seed.example/files/".to_string(),
                "https://seed.example/mirror/".to_string(),
            ]
        );
    }

    #[test]
    fn directory_output_name_is_rejected_before_io() {
        let options = DownloadOptions {
            out: Some(".".to_string()),
            ..DownloadOptions::default()
        };

        let error =
            match MagnetDownloadCommand::new(GroupId::new(5), TEST_MAGNET_URI, &options, None) {
                Ok(_) => panic!("a directory cannot be a magnet output file name"),
                Err(error) => error,
            };

        assert!(
            error
                .to_string()
                .contains("output name must be a file name")
        );
    }

    #[test]
    fn unsafe_display_name_falls_back_to_a_file_name() {
        let command = MagnetDownloadCommand::new(
            GroupId::new(6),
            "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=.",
            &DownloadOptions::default(),
            Some("."),
        )
        .expect("unsafe display names should use the fallback");

        assert_eq!(
            command.output_path,
            std::path::PathBuf::from(".").join("magnet_download")
        );
    }

    #[test]
    fn saved_metadata_is_loaded_only_when_info_hash_matches() {
        let temp_dir = tempfile::tempdir().expect("temporary metadata directory");
        let command = make_test_command_in_dir(temp_dir.path());
        let torrent = build_test_torrent();
        let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent parses")
            .info_hash
            .bytes;
        let path = command.saved_metadata_path(&info_hash);
        std::fs::write(&path, &torrent).expect("write saved metadata");

        assert_eq!(command.load_saved_metadata(&info_hash), Some(torrent));
        let mismatched_path = command.saved_metadata_path(&[0u8; 20]);
        std::fs::write(&mismatched_path, build_test_torrent())
            .expect("write mismatched saved metadata");
        assert!(command.load_saved_metadata(&[0u8; 20]).is_none());
    }

    #[tokio::test]
    async fn magnet_metadata_options_drive_saved_load_and_metadata_only_execution() {
        let temp_dir = tempfile::tempdir().expect("temporary metadata directory");
        let torrent = build_test_torrent();
        let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent parses")
            .info_hash
            .bytes;
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&dn=test_file",
            hex::encode(info_hash)
        );
        let options = DownloadOptions {
            bt_load_saved_metadata: true,
            bt_save_metadata: true,
            bt_metadata_only: true,
            enable_dht: false,
            ..DownloadOptions::default()
        };
        let mut command = MagnetDownloadCommand::new(
            GroupId::new(3),
            &magnet,
            &options,
            temp_dir.path().to_str(),
        )
        .expect("magnet command should be constructible");
        let saved_path = command.saved_metadata_path(&info_hash);
        std::fs::write(&saved_path, &torrent).expect("write saved torrent metadata");

        let listener = Arc::new(MetadataEventListener::new());
        DownloadEventHooks::shared().add_listener(listener.clone());

        command
            .execute()
            .await
            .expect("saved metadata should avoid network discovery");

        assert_eq!(command.status(), CommandStatus::Completed);
        assert_eq!(
            command.group().status(),
            crate::request::request_group::DownloadStatus::Complete
        );
        assert_eq!(
            std::fs::read(saved_path).expect("read saved torrent"),
            torrent
        );
        assert_eq!(
            listener.events.lock().unwrap().as_slice(),
            [MetadataResolvedEvent::new(GroupId::new(3), Vec::new())]
        );
        listener
            .alive
            .store(false, std::sync::atomic::Ordering::Release);
    }

    #[test]
    fn saving_metadata_never_overwrites_an_existing_file() {
        let temp_dir = tempfile::tempdir().expect("temporary metadata directory");
        let path = temp_dir.path().join("metadata.torrent");
        std::fs::write(&path, b"original").expect("write existing metadata");

        assert!(
            !MagnetDownloadCommand::save_metadata_file(&path, b"replacement")
                .expect("create_new should not fail for an existing file")
        );
        assert_eq!(
            std::fs::read(&path).expect("read existing metadata"),
            b"original"
        );

        let new_path = temp_dir.path().join("new.torrent");
        assert!(
            MagnetDownloadCommand::save_metadata_file(&new_path, b"metadata")
                .expect("write new metadata")
        );
        assert_eq!(
            std::fs::read(new_path).expect("read new metadata"),
            b"metadata"
        );
    }

    #[test]
    fn metadata_only_command_reports_completion_without_payload_bytes() {
        let mut command = make_test_command();
        assert_eq!(command.status(), CommandStatus::Pending);
        command.metadata_complete = true;
        assert_eq!(command.status(), CommandStatus::Completed);
    }

    /// Start a real DhtEngine on an ephemeral port for testing.
    ///
    /// Uses `DhtEngineConfig::local()`: an OS-assigned port (avoids conflicts)
    /// and no public bootstrap, so the test performs no outbound network I/O
    /// and cannot stall on DNS or unreachable entry points.
    async fn start_test_dht_engine()
    -> std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine> {
        aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
        )
        .await
        .expect("Failed to start DhtEngine for test")
    }

    /// BEP 0027: After metadata exchange, a private torrent must cause the
    /// DHT engine (started for peer discovery) to be shut down and the
    /// family-scoped `dht_engines` set emptied so the downstream `BtDownloadCommand`
    /// cannot accidentally reuse it.
    #[tokio::test]
    async fn test_magnet_private_torrent_dht_shutdown_after_metadata() {
        let mut cmd = make_test_command();
        cmd.dht_engines.insert(start_test_dht_engine().await);
        assert!(
            !cmd.dht_engines.is_empty(),
            "precondition: DHT engine present"
        );

        let torrent_bytes = build_private_test_torrent();

        cmd.enforce_bep0027_after_metadata(&torrent_bytes)
            .await
            .expect("enforce_bep0027 should succeed for private torrent");

        assert!(
            cmd.dht_engines.is_empty(),
            "DHT engines must be empty after private torrent metadata (BEP 0027)"
        );
    }

    /// BEP 0027: A public torrent (no `private` flag) must NOT trigger DHT
    /// shutdown — the DHT engine started for peer discovery remains active
    /// so the downstream download can continue using it.
    #[tokio::test]
    async fn test_magnet_public_torrent_dht_continues() {
        let mut cmd = make_test_command();
        let engine = start_test_dht_engine().await;
        cmd.dht_engines.insert(Arc::clone(&engine));

        let torrent_bytes = build_test_torrent();

        cmd.enforce_bep0027_after_metadata(&torrent_bytes)
            .await
            .expect("enforce_bep0027 should succeed for public torrent");

        assert!(
            !cmd.dht_engines.is_empty(),
            "DHT engine must remain active for public torrent"
        );

        // Clean up: shut down the still-running engine to release the socket.
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn private_magnet_does_not_shutdown_a_shared_global_dht_engine() {
        let mut cmd = make_test_command();
        let registry = std::sync::Arc::new(std::sync::RwLock::new(
            crate::engine::bittorrent::registry::BtRegistry::new(),
        ));
        let engine = start_test_dht_engine().await;
        registry
            .write()
            .expect("BT registry should be writable")
            .set_global_dht_engine(std::sync::Arc::clone(&engine));
        cmd.set_bt_registry(std::sync::Arc::clone(&registry));
        let gid = cmd.group().gid().value();
        assert!(
            cmd.dht_engines
                .attach_global_if_present(Some(&registry), gid, false,)
        );

        cmd.enforce_bep0027_after_metadata(&build_private_test_torrent())
            .await
            .expect("private metadata should be accepted");

        assert!(cmd.dht_engines.is_empty());
        assert_eq!(
            engine.state().await,
            aria2_protocol::bittorrent::dht::engine::DhtEngineState::Running
        );
        assert!(
            registry
                .read()
                .expect("BT registry should be readable")
                .get_global_dht_engine_for_peer(engine.local_addr())
                .is_some_and(|global| std::sync::Arc::ptr_eq(&global, &engine))
        );

        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn public_magnet_hands_its_dht_engine_to_the_payload_command() {
        use crate::engine::bittorrent::download::command::BtDownloadCommand;

        let mut cmd = make_test_command();
        let registry = std::sync::Arc::new(std::sync::RwLock::new(
            crate::engine::bittorrent::registry::BtRegistry::new(),
        ));
        cmd.set_bt_registry(std::sync::Arc::clone(&registry));
        let gid = cmd.group().gid().value();
        cmd.dht_engines
            .start_or_join(
                Some(&registry),
                gid,
                aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
            )
            .await
            .expect("magnet should start and register its shared DHT engine");
        let engine = cmd
            .dht_engines
            .ipv4()
            .expect("magnet should retain the IPv4 DHT handle");

        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let mut payload = BtDownloadCommand::new(GroupId::new(1), &torrent, &options, None)
            .expect("public torrent should construct");
        payload.set_bt_registry(std::sync::Arc::clone(&registry));

        cmd.handoff_dht_engine_to_bt(&mut payload).await;

        let transferred = payload
            .dht_engines
            .ipv4()
            .expect("payload command should receive the DHT engine");
        assert!(std::sync::Arc::ptr_eq(&transferred, &engine));
        assert!(cmd.dht_engines.is_empty());
        assert!(
            registry
                .read()
                .expect("BT registry should be readable")
                .get_global_dht_engine_for_peer(engine.local_addr())
                .is_some_and(|global| std::sync::Arc::ptr_eq(&global, &engine))
        );

        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn public_magnet_prefers_an_existing_global_dht_engine_at_handoff() {
        use crate::engine::bittorrent::download::command::BtDownloadCommand;

        let mut cmd = make_test_command();
        let registry = std::sync::Arc::new(std::sync::RwLock::new(
            crate::engine::bittorrent::registry::BtRegistry::new(),
        ));
        let global_engine = start_test_dht_engine().await;
        registry
            .write()
            .expect("BT registry should be writable")
            .set_global_dht_engine(std::sync::Arc::clone(&global_engine));
        cmd.set_bt_registry(std::sync::Arc::clone(&registry));
        cmd.ensure_dht_engines(&DownloadOptions {
            enable_dht: true,
            ..DownloadOptions::default()
        })
        .await
        .expect("magnet should attach to the existing family engine");
        assert!(
            cmd.dht_engines
                .ipv4()
                .as_ref()
                .is_some_and(|engine| std::sync::Arc::ptr_eq(engine, &global_engine))
        );

        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let mut payload = BtDownloadCommand::new(GroupId::new(1), &torrent, &options, None)
            .expect("public torrent should construct");
        payload.set_bt_registry(std::sync::Arc::clone(&registry));

        cmd.handoff_dht_engine_to_bt(&mut payload).await;

        assert_eq!(
            global_engine.state().await,
            aria2_protocol::bittorrent::dht::engine::DhtEngineState::Running
        );
        assert!(
            payload
                .dht_engines
                .ipv4()
                .as_ref()
                .is_some_and(|engine| std::sync::Arc::ptr_eq(engine, &global_engine))
        );

        global_engine.shutdown_async().await;
    }

    /// When DHT was never started (e.g. enable_dht = false), the enforcement
    /// method must still parse the metadata and succeed without error. There
    /// is nothing to shut down, so the DHT engine set stays empty.
    #[tokio::test]
    async fn test_magnet_enforce_bep0027_no_dht_engine() {
        let mut cmd = make_test_command();
        assert!(cmd.dht_engines.is_empty(), "precondition: no DHT engine");

        let torrent_bytes = build_private_test_torrent();

        cmd.enforce_bep0027_after_metadata(&torrent_bytes)
            .await
            .expect("should succeed even when DHT engine is absent");

        assert!(cmd.dht_engines.is_empty());
    }

    /// Corrupt metadata bytes must produce a fatal config error rather than
    /// silently treating the torrent as public (which would leak DHT usage
    /// for what might actually be a private torrent).
    #[tokio::test]
    async fn test_magnet_enforce_bep0027_invalid_metadata_errors() {
        let mut cmd = make_test_command();

        let bad_bytes: &[u8] = b"this is not valid bencode";

        let result = cmd.enforce_bep0027_after_metadata(bad_bytes).await;
        assert!(
            result.is_err(),
            "Invalid metadata bytes must return an error, not silently default to public"
        );

        // DHT engine should be untouched when parsing fails (fail-closed on
        // the parse error, but we do not preemptively shut down DHT since the
        // caller may want to retry metadata fetch from a different peer).
        assert!(
            cmd.dht_engines.is_empty(),
            "DHT engine set should be unchanged on parse error"
        );
    }
}
