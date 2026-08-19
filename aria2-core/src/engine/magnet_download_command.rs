use async_trait::async_trait;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::engine::metadata_exchange::{MetadataExchangeConfig, MetadataExchangeSession};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

pub struct MagnetDownloadCommand {
    group: Arc<std::sync::RwLock<RequestGroup>>,
    magnet_uri: String,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    metadata_complete: bool,
    dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// Carried through to the internally-created `BtDownloadCommand`.
    global_limiter: Option<RateLimiter>,
    #[cfg(feature = "bittorrent")]
    public_tracker_catalog:
        Option<Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    #[cfg(feature = "bittorrent")]
    bt_listener: Option<Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>>,
    #[cfg(feature = "bittorrent")]
    bt_registry: Option<Arc<std::sync::RwLock<crate::engine::bt_registry::BtRegistry>>>,
    #[cfg(feature = "bittorrent")]
    lpd_manager: Option<Arc<crate::engine::lpd_manager::LpdManager>>,
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

        let filename = _ml
            .display_name
            .as_deref()
            .unwrap_or("magnet_download")
            .to_string();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        info!(
            "MagnetDownloadCommand created (shared group): {} -> {} (hash={})",
            filename,
            path.display(),
            _ml.info_hash_hex()
        );

        Ok(Self {
            group,
            magnet_uri,
            output_path: path,
            started: false,
            completed_bytes: 0,
            metadata_complete: false,
            dht_engine: None,
            global_limiter: None,
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
        listener: Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>,
    ) {
        self.bt_listener = Some(listener);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_registry(
        &mut self,
        registry: Arc<std::sync::RwLock<crate::engine::bt_registry::BtRegistry>>,
    ) {
        self.bt_registry = Some(registry);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_lpd_manager(&mut self, manager: Arc<crate::engine::lpd_manager::LpdManager>) {
        self.lpd_manager = Some(manager);
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    fn saved_metadata_path(&self, info_hash: &[u8; 20]) -> std::path::PathBuf {
        self.output_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}.torrent", hex::encode(info_hash)))
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

        if enable_dht && self.dht_engine.is_none() {
            let dht_config = crate::engine::dht_config::build_dht_engine_config(&options).await?;
            match aria2_protocol::bittorrent::dht::engine::DhtEngine::start(dht_config).await {
                Ok(engine) => {
                    self.dht_engine = Some(engine);
                    self.dht_engine.as_ref().unwrap().start_maintenance_loop();
                    info!("Magnet: DHT engine started for peer discovery");
                }
                Err(error) => {
                    warn!("Magnet: DHT engine start failed: {}", error);
                }
            }
        }

        let discovered_peers = if let Some(ref engine) = self.dht_engine {
            match engine.find_peers(&magnet.info_hash).await {
                Ok(result) => {
                    info!(
                        "Magnet: DHT discovered {} peers (contacted {} nodes)",
                        result.peers.len(),
                        result.nodes_contacted
                    );
                    result.peers
                }
                Err(error) => {
                    warn!("Magnet: DHT find_peers failed: {}", error);
                    vec![]
                }
            }
        } else {
            warn!("Magnet: DHT disabled, no peers available");
            vec![]
        };

        if discovered_peers.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "No peers found via DHT".into(),
                },
            ));
        }

        let meta_session = MetadataExchangeSession::new(MetadataExchangeConfig {
            max_peers_to_try: discovered_peers.len().min(5),
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(10),
            piece_size: 16 * 1024,
            ..MetadataExchangeConfig::default()
        });

        meta_session
            .fetch_metadata(&magnet.info_hash, &discovered_peers)
            .await
            .map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Metadata fetch failed: {}", error),
                })
            })
    }

    /// BEP 0027 (Private Torrent) enforcement after metadata exchange.
    ///
    /// For magnet links, the DHT engine is started BEFORE metadata arrives
    /// because DHT-based peer discovery is required to find peers that can
    /// serve the metadata via BEP 0010 (Extension for Peers to Send Metadata
    /// File). Once the metadata has been fetched, if the torrent's `private`
    /// flag is set, DHT must be shut down to comply with BEP 0027 which
    /// forbids DHT, PEX, and LPD for private torrents.
    ///
    /// This method parses the fetched `torrent_bytes`, checks `is_private()`,
    /// and if true, shuts down the DHT engine asynchronously and clears the
    /// `dht_engine` field so the downstream `BtDownloadCommand` (created from
    /// the same bytes) will not see a running DHT engine and will itself
    /// honour BEP 0027 by not starting a new one.
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
            if let Some(ref engine) = self.dht_engine {
                engine.shutdown_async().await;
            }
            // Clear the field so the trailing shutdown() call in execute()
            // does not attempt to shut down an already-stopped engine, and
            // so the downstream BtDownloadCommand cannot accidentally reuse
            // the engine.
            self.dht_engine = None;
        }

        Ok(())
    }
}

#[async_trait]
impl Command for MagnetDownloadCommand {
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
            if let Some(data) = self.load_saved_metadata(&ml.info_hash) {
                data
            } else {
                self.fetch_magnet_metadata(&ml).await?
            }
        } else {
            self.fetch_magnet_metadata(&ml).await?
        };

        info!("Fetched torrent metadata: {} bytes", torrent_bytes.len());

        // BEP 0027 (Private Torrent): DHT was started before metadata exchange
        // for peer discovery. Now that the metadata is available, parse it and
        // shut down DHT if the torrent is private. The downstream
        // BtDownloadCommand (created from the same bytes) will also enforce
        // BEP 0027 by refusing to start its own DHT when is_private is set.
        self.enforce_bep0027_after_metadata(&torrent_bytes).await?;

        if save_metadata {
            let path = self.saved_metadata_path(&ml.info_hash);
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
            self.group.recover_mut().complete()?;
            self.metadata_complete = true;
            info!("Magnet metadata download complete; payload download skipped");
            return Ok(());
        }

        use crate::engine::bt_download_command::BtDownloadCommand;
        let mut bt_cmd = BtDownloadCommand::new(
            self.group.recover().gid(),
            &torrent_bytes,
            self.group.recover().options(),
            self.output_path.parent().and_then(|p| p.to_str()),
        )?;
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

        bt_cmd.execute().await?;

        if let Some(ref engine) = self.dht_engine {
            engine.shutdown();
        }

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
    use crate::engine::bt_download_command_tests::{
        build_private_test_torrent, build_test_torrent,
    };

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
    /// `dht_engine` field cleared so the downstream `BtDownloadCommand`
    /// cannot accidentally reuse it.
    #[tokio::test]
    async fn test_magnet_private_torrent_dht_shutdown_after_metadata() {
        let mut cmd = make_test_command();
        cmd.dht_engine = Some(start_test_dht_engine().await);
        assert!(cmd.dht_engine.is_some(), "precondition: DHT engine present");

        let torrent_bytes = build_private_test_torrent();

        cmd.enforce_bep0027_after_metadata(&torrent_bytes)
            .await
            .expect("enforce_bep0027 should succeed for private torrent");

        assert!(
            cmd.dht_engine.is_none(),
            "DHT engine must be None after private torrent metadata (BEP 0027)"
        );
    }

    /// BEP 0027: A public torrent (no `private` flag) must NOT trigger DHT
    /// shutdown — the DHT engine started for peer discovery remains active
    /// so the downstream download can continue using it.
    #[tokio::test]
    async fn test_magnet_public_torrent_dht_continues() {
        let mut cmd = make_test_command();
        let engine = start_test_dht_engine().await;
        cmd.dht_engine = Some(engine.clone());

        let torrent_bytes = build_test_torrent();

        cmd.enforce_bep0027_after_metadata(&torrent_bytes)
            .await
            .expect("enforce_bep0027 should succeed for public torrent");

        assert!(
            cmd.dht_engine.is_some(),
            "DHT engine must remain active for public torrent"
        );

        // Clean up: shut down the still-running engine to release the socket.
        engine.shutdown_async().await;
    }

    /// When DHT was never started (e.g. enable_dht = false), the enforcement
    /// method must still parse the metadata and succeed without error. There
    /// is nothing to shut down, so `dht_engine` stays `None`.
    #[tokio::test]
    async fn test_magnet_enforce_bep0027_no_dht_engine() {
        let mut cmd = make_test_command();
        assert!(cmd.dht_engine.is_none(), "precondition: no DHT engine");

        let torrent_bytes = build_private_test_torrent();

        cmd.enforce_bep0027_after_metadata(&torrent_bytes)
            .await
            .expect("should succeed even when DHT engine is absent");

        assert!(cmd.dht_engine.is_none());
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
            cmd.dht_engine.is_none(),
            "DHT engine field should be unchanged on parse error"
        );
    }
}
