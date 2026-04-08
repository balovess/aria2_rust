use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use tracing::{info, debug, warn};

use crate::error::{Aria2Error, Result, RecoverableError, FatalError};
use crate::engine::command::{Command, CommandStatus};
use crate::request::request_group::{RequestGroup, GroupId, DownloadOptions};
use crate::engine::metadata_exchange::{MetadataExchangeSession, MetadataExchangeConfig};

pub struct MagnetDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    magnet_uri: String,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
}

impl MagnetDownloadCommand {
    pub fn new(
        gid: GroupId,
        magnet_uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let _ml = aria2_protocol::bittorrent::magnet::MagnetLink::parse(magnet_uri)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Invalid magnet link: {}", e))))?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = _ml.display_name.as_deref().unwrap_or("magnet_download").to_string();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        let urls = vec![magnet_uri.to_string()];
        let group = RequestGroup::new(gid, urls, options.clone());

        info!("MagnetDownloadCommand created: {} -> {} (hash={})", filename, path.display(), _ml.info_hash_hex());

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            magnet_uri: magnet_uri.to_string(),
            output_path: path,
            started: false,
            completed_bytes: 0,
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

        let ml = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&self.magnet_uri)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Magnet parse error: {}", e))))?;

        info!("Magnet download: hash={}, name={:?}", ml.info_hash_hex(), ml.display_name);

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e))))?;
            }
        }

        use aria2_protocol::bittorrent::dht::client::{
            DhtClient, DhtClientConfig, generate_random_node_id,
        };
        use aria2_protocol::bittorrent::dht::bootstrap::DhtBootstrap;

        let dht_config = DhtClientConfig {
            self_id: generate_random_node_id(),
            bootstrap_nodes: DhtBootstrap::get_bootstrap_nodes()
                .iter().map(|n| n.addr).collect(),
            max_concurrent_queries: 8,
            query_timeout: Duration::from_secs(5),
            max_rounds: 3,
        };
        let mut dht_client = DhtClient::new(dht_config);

        let discovered = tokio::time::timeout(
            Duration::from_secs(30),
            dht_client.discover_peers(&ml.info_hash),
        ).await
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "DHT discovery timeout".into(),
        }))?
        .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("DHT peer discovery failed: {}", e),
        }))?;

        info!("DHT discovered {} peers", discovered.addresses.len());

        if discovered.addresses.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "No peers found via DHT".into(),
                }
            ));
        }

        let meta_session = MetadataExchangeSession::new(MetadataExchangeConfig {
            max_peers_to_try: discovered.addresses.len().min(5),
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(10),
            piece_size: 16 * 1024,
        });

        let torrent_bytes = meta_session.fetch_metadata(&ml.info_hash, &discovered.addresses).await
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Metadata fetch failed: {}", e),
            }))?;

        info!("Fetched torrent metadata: {} bytes", torrent_bytes.len());

        use crate::engine::bt_download_command::BtDownloadCommand;
        let mut bt_cmd = BtDownloadCommand::new(
            self.group.read().await.gid(),
            &torrent_bytes,
            &DownloadOptions::default(),
            self.output_path.parent().and_then(|p| p.to_str()),
        )?;

        bt_cmd.execute().await?;

        self.completed_bytes = self.group.read().await.total_length();

        info!("Magnet download complete: {}", self.output_path.display());
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 { CommandStatus::Running } else { CommandStatus::Pending }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(900))
    }
}
