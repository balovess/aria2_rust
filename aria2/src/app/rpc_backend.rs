//! Core-owned implementation of the protocol-independent RPC backend.
//!
//! `aria2-rpc` owns the wire protocol and knows only [`RpcBackend`].  This
//! adapter is the single place where RPC operations are translated into
//! `aria2-core` state changes, queries, and engine commands.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aria2_core::checksum::checksum::Checksum;
use aria2_core::config::{ConfigManager, project_initial_options};
#[cfg(feature = "bittorrent")]
use aria2_core::engine::bt_registry::BtRegistry;
use aria2_core::engine::engine_command::{EngineCommand, EngineCommandSender};
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::{BackendError, BackendMetadata};
use tokio::sync::RwLock;

mod creation;
mod dispatch;
mod lifecycle;
mod query;
mod values;

const RPC_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

fn rpc_peer_port(addr: SocketAddr, is_incoming: bool) -> u16 {
    if is_incoming { 0 } else { addr.port() }
}

/// The application adapter behind the RPC wire layer.
pub struct CoreRpcBackend {
    group_man: Arc<RequestGroupMan>,
    engine_cmd_tx: EngineCommandSender,
    config: Arc<RwLock<ConfigManager>>,
    save_session_path: Option<PathBuf>,
    metadata: BackendMetadata,
    #[cfg(feature = "bittorrent")]
    bt_registry: Option<Arc<std::sync::RwLock<BtRegistry>>>,
}
impl CoreRpcBackend {
    pub fn new(
        group_man: Arc<RequestGroupMan>,
        engine_cmd_tx: EngineCommandSender,
        config: Arc<RwLock<ConfigManager>>,
        save_session_path: Option<PathBuf>,
        product_version: impl Into<String>,
    ) -> Self {
        let mut metadata = BackendMetadata::base(product_version);
        #[cfg(feature = "bittorrent")]
        {
            metadata = metadata.with_bittorrent();
        }
        #[cfg(feature = "metalink")]
        {
            metadata = metadata.with_metalink();
        }
        #[cfg(feature = "sftp")]
        {
            metadata = metadata.with_sftp();
        }

        Self {
            group_man,
            engine_cmd_tx,
            config,
            save_session_path,
            metadata,
            #[cfg(feature = "bittorrent")]
            bt_registry: None,
        }
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_registry(&mut self, registry: Arc<std::sync::RwLock<BtRegistry>>) {
        self.bt_registry = Some(registry);
    }

    fn invalid(message: impl Into<String>) -> BackendError {
        BackendError::InvalidParams(message.into())
    }

    fn execution(message: impl Into<String>) -> BackendError {
        BackendError::Execution(message.into())
    }

    #[cfg(feature = "bittorrent")]
    fn validate_torrent_data(data: &[u8]) -> Result<(), BackendError> {
        if data.len() < 3 || data[0] != b'd' || data[1] != b'8' || data[2] != b':' {
            return Err(Self::invalid("Invalid BEncode data (not a .torrent file)"));
        }
        Ok(())
    }

    fn parse_gid(gid: &str) -> Result<GroupId, BackendError> {
        GroupId::from_hex_string(gid).ok_or_else(|| Self::invalid("Invalid GID"))
    }

    fn send(&self, command: EngineCommand) -> Result<(), BackendError> {
        self.engine_cmd_tx.send(command).map_err(|error| {
            BackendError::Internal(format!("Failed to send engine command: {error}"))
        })
    }

    fn group(&self, gid: &str) -> Result<Arc<std::sync::RwLock<RequestGroup>>, BackendError> {
        self.group_man
            .group_by_hex(gid)
            .ok_or_else(|| Self::execution(format!("GID {gid} not found")))
    }

    async fn global_options(&self) -> HashMap<String, serde_json::Value> {
        self.config
            .read()
            .await
            .get_all_global_options()
            .await
            .into_iter()
            .map(|(key, value)| (key, (&value).into()))
            .collect()
    }

    async fn merged_task_options(
        &self,
        request_options: HashMap<String, serde_json::Value>,
    ) -> Result<(DownloadOptions, HashMap<String, serde_json::Value>), BackendError> {
        let mut options = self.global_options().await;
        options.extend(request_options);
        let download_options =
            DownloadOptions::try_from_rpc_options(&options).map_err(Self::invalid)?;
        let snapshot = project_initial_options(options);
        if let Some((algorithm, value)) = &download_options.checksum {
            Checksum::from_type_and_value(algorithm, value)
                .map_err(|error| Self::invalid(format!("Invalid checksum: {error}")))?;
        }
        Ok((download_options, snapshot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_uses_real_non_bt_connection_count_instead_of_split() {
        let options = DownloadOptions {
            split: Some(16),
            ..DownloadOptions::default()
        };
        let group = RequestGroup::new(GroupId::new(0x101), Vec::new(), options);
        group.set_stream_connection_count(1);

        let status = CoreRpcBackend::status_from_group(&group, "0000000000000101");

        assert_eq!(status.connections, Some(1));
    }

    #[test]
    fn incoming_bt_peer_reports_aria2_compatible_zero_port() {
        let incoming_addr = "127.0.0.1:7673".parse().expect("valid socket address");
        assert_eq!(rpc_peer_port(incoming_addr, true), 0);
        assert_eq!(rpc_peer_port(incoming_addr, false), 7673);
    }
}
