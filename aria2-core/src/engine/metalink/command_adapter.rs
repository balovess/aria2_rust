#[cfg(feature = "bittorrent")]
use std::sync::Arc;

use async_trait::async_trait;

use crate::engine::command::Command;
use crate::engine::protocol_adapter::{
    ProtocolCommandAdapter, ProtocolCommandRequest, ProtocolServices,
};
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

pub(crate) struct MetalinkCommandAdapter {
    #[cfg(feature = "bittorrent")]
    bittorrent: crate::engine::bittorrent::command_adapter::BtCommandServices,
}

impl MetalinkCommandAdapter {
    pub(crate) fn new(
        #[cfg(feature = "bittorrent")]
        bittorrent: crate::engine::bittorrent::command_adapter::BtCommandServices,
    ) -> Self {
        Self {
            #[cfg(feature = "bittorrent")]
            bittorrent,
        }
    }
}

#[async_trait]
impl ProtocolCommandAdapter for MetalinkCommandAdapter {
    fn supports(&self, request: &ProtocolCommandRequest) -> bool {
        request.group.recover().metalink_source().is_some()
    }

    async fn create(
        &self,
        request: ProtocolCommandRequest,
        services: &ProtocolServices,
    ) -> Result<Box<dyn Command>> {
        let (metalink_data, file_index) = request
            .group
            .recover()
            .metalink_source()
            .expect("the registry calls this adapter only for Metalink groups");
        let base_uri = request.group.recover().metalink_base_uri();
        let mut command =
            crate::engine::metalink::download_command::MetalinkDownloadCommand::new_with_group_source(
                request.group,
                &metalink_data,
                file_index,
                &request.options,
                base_uri.as_deref(),
                &services.outbound_network_policy,
            )?;
        if let Some(limiter) = services.global_limiter.clone() {
            command.set_global_limiter(limiter);
        }
        #[cfg(feature = "bittorrent")]
        {
            command.set_public_tracker_catalog(Arc::clone(&self.bittorrent.public_tracker_catalog));
            command.set_bt_registry(Arc::clone(&self.bittorrent.bt_registry));
            command.set_bt_listener(Arc::clone(&self.bittorrent.bt_listener));
            command.set_lpd_manager(Arc::clone(&self.bittorrent.lpd_manager));
        }
        Ok(Box::new(command))
    }
}
