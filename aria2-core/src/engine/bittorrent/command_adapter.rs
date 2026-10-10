use std::sync::Arc;

use async_trait::async_trait;

use crate::engine::command::Command;
use crate::engine::protocol_adapter::{
    ProtocolCommandAdapter, ProtocolCommandRequest, ProtocolServices,
};
use crate::error::{Aria2Error, FatalError, Result};
use crate::util::rwlock_ext::RwLockRecover;

#[derive(Clone)]
pub(crate) struct BtCommandServices {
    pub(crate) public_tracker_catalog:
        Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>,
    pub(crate) bt_registry: Arc<std::sync::RwLock<crate::engine::bittorrent::registry::BtRegistry>>,
    pub(crate) bt_listener: Arc<crate::engine::bittorrent::peer::listener::BtPeerListenerManager>,
    pub(crate) lpd_manager: Arc<crate::engine::bittorrent::discovery::lpd::LpdManager>,
}

pub(crate) struct BtCommandAdapter {
    services: BtCommandServices,
}

impl BtCommandAdapter {
    pub(crate) fn new(services: BtCommandServices) -> Self {
        Self { services }
    }
}

#[async_trait]
impl ProtocolCommandAdapter for BtCommandAdapter {
    fn supports(&self, request: &ProtocolCommandRequest) -> bool {
        let uri = request.first_uri.to_ascii_lowercase();
        uri.starts_with("bt://")
            || uri.starts_with("magnet:")
            || request.group.recover().bt_metadata_data().is_some()
    }

    async fn create(
        &self,
        request: ProtocolCommandRequest,
        services: &ProtocolServices,
    ) -> Result<Box<dyn Command>> {
        let uri_lower = request.first_uri.to_ascii_lowercase();
        let bt_metadata = request.group.recover().bt_metadata_data();
        if uri_lower.starts_with("bt://") || bt_metadata.is_some() {
            let torrent_bytes = bt_metadata
                .or_else(|| {
                    request
                        .group
                        .recover()
                        .metadata_info()
                        .and_then(|info| info.metadata_path().map(std::path::PathBuf::from))
                        .and_then(|path| std::fs::read(path).ok())
                })
                .ok_or_else(|| {
                    Aria2Error::Fatal(FatalError::Config(
                        "Resolved BitTorrent payload has no metadata source".to_string(),
                    ))
                })?;
            let mut command = crate::engine::bittorrent::download::command::BtDownloadCommand::new_with_group_and_mappings_with_policy(
                request.group,
                &torrent_bytes,
                &request.options,
                request.options.dir.as_deref(),
                &[],
                &services.outbound_network_policy,
            )?;
            command.set_bt_listener(Arc::clone(&self.services.bt_listener));
            command.set_bt_registry(Arc::clone(&self.services.bt_registry));
            command.set_lpd_manager(Arc::clone(&self.services.lpd_manager));
            if let Some(limiter) = services.global_limiter.clone() {
                command.set_global_limiter(limiter);
            }
            command.set_public_tracker_catalog(Arc::clone(&self.services.public_tracker_catalog));
            return Ok(Box::new(command));
        }

        let mut command = crate::engine::bittorrent::magnet::download_command::MagnetDownloadCommand::new_with_group(
            request.group,
            request.options.dir.as_deref(),
        )?;
        command.set_bt_listener(Arc::clone(&self.services.bt_listener));
        command.set_bt_registry(Arc::clone(&self.services.bt_registry));
        command.set_lpd_manager(Arc::clone(&self.services.lpd_manager));
        command.set_outbound_network_policy(Arc::clone(&services.outbound_network_policy));
        if let Some(limiter) = services.global_limiter.clone() {
            command.set_global_limiter(limiter);
        }
        command.set_public_tracker_catalog(Arc::clone(&self.services.public_tracker_catalog));
        Ok(Box::new(command))
    }
}
