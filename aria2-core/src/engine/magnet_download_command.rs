use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::engine::metadata_exchange::{MetadataExchangeConfig, MetadataExchangeSession};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

pub struct MagnetDownloadCommand {
    group: Arc<std::sync::RwLock<RequestGroup>>,
    magnet_uri: String,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
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

        let _ml =
            aria2_protocol::bittorrent::magnet::MagnetLink::parse(&magnet_uri).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("Invalid magnet link: {}", e)))
            })?;

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
            dht_engine: None,
        })
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
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

        let ml = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&self.magnet_uri).map_err(
            |e| Aria2Error::Fatal(FatalError::Config(format!("Magnet parse error: {}", e))),
        )?;

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

        let enable_dht = { self.group.recover().options().enable_dht };
        let dht_port = { self.group.recover().options().dht_listen_port };

        if enable_dht && self.dht_engine.is_none() {
            let dht_config = aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
                port: dht_port.unwrap_or(0),
                ..Default::default()
            };
            match aria2_protocol::bittorrent::dht::engine::DhtEngine::start(dht_config).await {
                Ok(engine) => {
                    self.dht_engine = Some(engine);
                    self.dht_engine.as_ref().unwrap().start_maintenance_loop();
                    info!("Magnet: DHT engine started for peer discovery");
                }
                Err(e) => {
                    warn!("Magnet: DHT engine start failed: {}", e);
                }
            }
        }

        let discovered_peers = if let Some(ref engine) = self.dht_engine {
            match engine.find_peers(&ml.info_hash).await {
                Ok(result) => {
                    info!(
                        "Magnet: DHT discovered {} peers (contacted {} nodes)",
                        result.peers.len(),
                        result.nodes_contacted
                    );
                    result.peers
                }
                Err(e) => {
                    warn!("Magnet: DHT find_peers failed: {}", e);
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

        let torrent_bytes = meta_session
            .fetch_metadata(&ml.info_hash, &discovered_peers)
            .await
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Metadata fetch failed: {}", e),
                })
            })?;

        info!("Fetched torrent metadata: {} bytes", torrent_bytes.len());

        // BEP 0027 (Private Torrent): DHT was started before metadata exchange
        // for peer discovery. Now that the metadata is available, parse it and
        // shut down DHT if the torrent is private. The downstream
        // BtDownloadCommand (created from the same bytes) will also enforce
        // BEP 0027 by refusing to start its own DHT when is_private is set.
        self.enforce_bep0027_after_metadata(&torrent_bytes).await?;

        use crate::engine::bt_download_command::BtDownloadCommand;
        let mut bt_cmd = BtDownloadCommand::new(
            self.group.recover().gid(),
            &torrent_bytes,
            &DownloadOptions::default(),
            self.output_path.parent().and_then(|p| p.to_str()),
        )?;

        bt_cmd.execute().await?;

        if let Some(ref engine) = self.dht_engine {
            engine.shutdown();
        }

        self.completed_bytes = self.group.recover().total_length();

        info!("Magnet download complete: {}", self.output_path.display());
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(900))
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

    /// Start a real DhtEngine on an ephemeral port (port 0) for testing.
    async fn start_test_dht_engine()
    -> std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine> {
        aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
                port: 0, // OS-assigned ephemeral port to avoid conflicts
                ..Default::default()
            },
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
