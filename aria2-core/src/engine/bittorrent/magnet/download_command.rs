mod discovery;
mod metadata;
mod metadata_source;

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::command::{Command, CommandStatus};
use crate::engine::download_event_hooks::{DownloadEventHooks, MetadataResolvedEvent};
use crate::error::{Aria2Error, FatalError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

const MAX_MAGNET_TRACKERS_TO_TRY: usize = 10;
const MAX_MAGNET_TRACKER_PEERS: usize = 50;
const MAX_MAGNET_METADATA_PEERS_PER_SOURCE: usize = 20;

pub struct MagnetDownloadCommand {
    group: Arc<std::sync::RwLock<RequestGroup>>,
    magnet_uri: String,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    metadata_complete: bool,
    local_peer_id: [u8; 20],
    #[cfg(feature = "bittorrent")]
    initial_peer_swarm: Option<crate::engine::bittorrent::peer::message_handler::PeerSwarm>,
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
            local_peer_id: aria2_protocol::bittorrent::peer::id::generate_peer_id_with_prefix(
                &options.peer_id_prefix,
            ),
            #[cfg(feature = "bittorrent")]
            initial_peer_swarm: None,
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
        #[cfg(feature = "bittorrent")]
        if let Some(mut swarm) = self.initial_peer_swarm.take() {
            swarm.shutdown_all().await;
        }
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

        let torrent_bytes = if load_saved_metadata
            && let Some(metadata) = self.load_saved_metadata_for_magnet(&ml)
        {
            metadata
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
            #[cfg(feature = "bittorrent")]
            if let Some(mut swarm) = self.initial_peer_swarm.take() {
                swarm.shutdown_all().await;
            }
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
        bt_cmd.local_peer_id = self.local_peer_id;
        bt_cmd.initial_peer_swarm = self.initial_peer_swarm.take();
        self.group.recover_mut().set_bt_metadata_data(torrent_bytes);
        DownloadEventHooks::shared()
            .notify_metadata_resolved(MetadataResolvedEvent::new(gid, vec![gid]));
        if let Some(gl) = self.global_limiter.clone() {
            bt_cmd.set_global_limiter(gl);
        }
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

        let download_result = bt_cmd.execute().await;
        if download_result.is_err() {
            bt_cmd.shutdown().await;
        }
        download_result?;

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
mod tests;
