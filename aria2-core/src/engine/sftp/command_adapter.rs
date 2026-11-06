use std::sync::Arc;

use async_trait::async_trait;

use crate::engine::command::Command;
use crate::engine::protocol_adapter::{
    ProtocolCommandAdapter, ProtocolCommandRequest, ProtocolServices,
};
use crate::error::Result;

pub(crate) struct SftpCommandAdapter;

#[async_trait]
impl ProtocolCommandAdapter for SftpCommandAdapter {
    fn supports(&self, request: &ProtocolCommandRequest) -> bool {
        request
            .first_uri
            .to_ascii_lowercase()
            .starts_with("sftp://")
    }

    async fn create(
        &self,
        request: ProtocolCommandRequest,
        services: &ProtocolServices,
    ) -> Result<Box<dyn Command>> {
        let mut command = super::download_command::SftpDownloadCommand::new_with_group(
            request.group,
            &request.first_uri,
            &request.options,
            request.options.dir.as_deref(),
            request.options.out.as_deref(),
        )?;
        command.set_outbound_network_policy(Arc::clone(&services.outbound_network_policy));
        if let Some(limiter) = services.global_limiter.clone() {
            command.set_global_limiter(limiter);
        }
        Ok(Box::new(command))
    }
}
