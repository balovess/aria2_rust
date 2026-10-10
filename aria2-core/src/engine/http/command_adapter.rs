use async_trait::async_trait;

use crate::engine::command::Command;
use crate::engine::protocol_adapter::{
    ProtocolCommandAdapter, ProtocolCommandRequest, ProtocolServices,
};
use crate::error::Result;

pub(crate) struct HttpCommandAdapter;

#[async_trait]
impl ProtocolCommandAdapter for HttpCommandAdapter {
    fn supports(&self, _request: &ProtocolCommandRequest) -> bool {
        true
    }

    async fn create(
        &self,
        request: ProtocolCommandRequest,
        services: &ProtocolServices,
    ) -> Result<Box<dyn Command>> {
        let command = super::command_factory::create_download_command(
            request.group,
            &request.first_uri,
            &request.options,
            std::sync::Arc::clone(&services.dns_cache),
            std::sync::Arc::clone(&services.outbound_network_policy),
            services.global_limiter.clone(),
        )
        .await?;

        Ok(Box::new(command))
    }
}
