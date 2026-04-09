use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::engine::metadata_exchange::{MetadataExchangeConfig, MetadataExchangeSession};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

pub struct MagnetDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
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
        let _ml =
            aria2_protocol::bittorrent::magnet::MagnetLink::parse(magnet_uri).map_err(|e| {
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

        let urls = vec![magnet_uri.to_string()];
        let group = RequestGroup::new(gid, urls, options.clone());

        info!(
            "MagnetDownloadCommand created: {} -> {} (hash={})",
            filename,
            path.display(),
            _ml.info_hash_hex()
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            magnet_uri: magnet_uri.to_string(),
            output_path: path,
            started: false,
            completed_bytes: 0,
            dht_engine: None,
        })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }
}

#[async_trait]
impl Command for MagnetDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
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

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
                })?;
            }
        }

        let enable_dht = { self.group.read().await.options().enable_dht };
        let dht_port = { self.group.read().await.options().dht_listen_port };

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
            let result = engine.find_peers(&ml.info_hash).await;
            info!(
                "Magnet: DHT discovered {} peers (contacted {} nodes)",
                result.peers.len(),
                result.nodes_contacted
            );
            result.peers
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

        use crate::engine::bt_download_command::BtDownloadCommand;
        let mut bt_cmd = BtDownloadCommand::new(
            self.group.read().await.gid(),
            &torrent_bytes,
            &DownloadOptions::default(),
            self.output_path.parent().and_then(|p| p.to_str()),
        )?;

        bt_cmd.execute().await?;

        if let Some(ref engine) = self.dht_engine {
            engine.shutdown();
        }

        self.completed_bytes = self.group.read().await.total_length();

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

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(900))
    }
}
