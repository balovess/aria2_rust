use std::sync::Arc;

use async_trait::async_trait;
use url::Url;

use crate::engine::command::Command;
use crate::engine::protocol_adapter::{
    ProtocolCommandAdapter, ProtocolCommandRequest, ProtocolServices,
};
use crate::error::Result;

pub(crate) struct FtpCommandAdapter;

fn ftp_origin(uri: &str) -> Option<(String, u16)> {
    let parsed = Url::parse(uri).ok()?;
    Some((
        parsed.host_str()?.to_string(),
        parsed.port_or_known_default()?,
    ))
}

#[async_trait]
impl ProtocolCommandAdapter for FtpCommandAdapter {
    fn supports(&self, request: &ProtocolCommandRequest) -> bool {
        let uri = request.first_uri.to_ascii_lowercase();
        uri.starts_with("ftp://") || uri.starts_with("ftps://")
    }

    async fn create(
        &self,
        request: ProtocolCommandRequest,
        services: &ProtocolServices,
    ) -> Result<Box<dyn Command>> {
        let mut command = super::download_command::FtpDownloadCommand::new_with_group(
            request.group,
            request.options.dir.as_deref(),
            request.options.out.as_deref(),
        )?;
        if let Some(limiter) = services.global_limiter.clone() {
            command.set_global_limiter(limiter);
        }
        command.set_outbound_network_policy(Arc::clone(&services.outbound_network_policy));
        if request.options.async_dns {
            command.set_dns_cache(Arc::clone(&services.dns_cache));
            if let Some((hostname, port)) = ftp_origin(&request.first_uri)
                && let Ok(addresses) = services
                    .dns_cache
                    .lock()
                    .await
                    .resolve_with_refresh(&hostname, port)
                    .await
            {
                command.set_resolved_addresses(addresses);
            }
        }

        Ok(Box::new(command))
    }
}
