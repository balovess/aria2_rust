//! Protocol-specific command construction behind one registry boundary.
//!
//! The engine loop owns task admission and lifecycle. This registry keeps
//! scheme and metadata dispatch out of that lifecycle path; protocol adapters
//! own construction of their command implementations.

use std::sync::Arc;

use async_trait::async_trait;

use crate::dns::dns_cache::DnsCache;
use crate::engine::command::Command;
use crate::engine::engine_loop::EngineLoopContext;
use crate::error::{Aria2Error, FatalError, Result};
use crate::network::OutboundNetworkPolicy;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, RequestGroup};

/// Per-task protocol input selected by the engine.
pub(crate) struct ProtocolCommandRequest {
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    pub(crate) first_uri: String,
    pub(crate) options: Arc<DownloadOptions>,
}

/// Shared, protocol-neutral services available to command adapters.
pub(crate) struct ProtocolServices {
    pub(crate) dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,
    pub(crate) outbound_network_policy: Arc<OutboundNetworkPolicy>,
    pub(crate) global_limiter: Option<RateLimiter>,
}

#[allow(clippy::double_must_use)]
#[async_trait]
pub(crate) trait ProtocolCommandAdapter: Send + Sync {
    /// Whether this adapter owns this task input. Earlier adapters have
    /// priority, so metadata-only formats can claim tasks before URI schemes.
    fn supports(&self, request: &ProtocolCommandRequest) -> bool;

    async fn create(
        &self,
        request: ProtocolCommandRequest,
        services: &ProtocolServices,
    ) -> Result<Box<dyn Command>>;
}

pub(crate) struct ProtocolAdapterRegistry {
    adapters: Vec<Box<dyn ProtocolCommandAdapter>>,
}

impl ProtocolAdapterRegistry {
    fn new(adapters: Vec<Box<dyn ProtocolCommandAdapter>>) -> Self {
        Self { adapters }
    }

    /// Assemble built-in adapters at the engine boundary. Protocol-specific
    /// shared state is injected into its owning adapter rather than carried by
    /// the generic task spawner.
    pub(crate) fn for_engine(_context: &EngineLoopContext) -> Self {
        #[cfg(feature = "bittorrent")]
        let bittorrent = crate::engine::bittorrent::command_adapter::BtCommandServices {
            public_tracker_catalog: Arc::clone(&_context.public_tracker_catalog),
            bt_registry: Arc::clone(&_context.bt_registry),
            bt_listener: Arc::clone(&_context.bt_listener),
            lpd_manager: Arc::clone(&_context.lpd_manager),
        };

        Self::builtins(
            #[cfg(feature = "bittorrent")]
            bittorrent,
        )
    }

    pub(crate) fn builtins(
        #[cfg(feature = "bittorrent")]
        bittorrent: crate::engine::bittorrent::command_adapter::BtCommandServices,
    ) -> Self {
        // Keep the always-available adapters in priority order; optional
        // formats are inserted ahead of their URI-scheme fallbacks below.
        let mut adapters: Vec<Box<dyn ProtocolCommandAdapter>> = vec![
            Box::new(crate::engine::ftp::command_adapter::FtpCommandAdapter),
            Box::new(crate::engine::http::command_adapter::HttpCommandAdapter),
        ];

        #[cfg(feature = "metalink")]
        adapters.insert(
            0,
            Box::new(
                crate::engine::metalink::command_adapter::MetalinkCommandAdapter::new(
                    #[cfg(feature = "bittorrent")]
                    bittorrent.clone(),
                ),
            ),
        );

        #[cfg(feature = "sftp")]
        adapters.insert(
            usize::from(cfg!(feature = "metalink")),
            Box::new(crate::engine::sftp::command_adapter::SftpCommandAdapter),
        );

        #[cfg(feature = "bittorrent")]
        adapters.insert(
            usize::from(cfg!(feature = "metalink")) + usize::from(cfg!(feature = "sftp")),
            Box::new(crate::engine::bittorrent::command_adapter::BtCommandAdapter::new(bittorrent)),
        );

        Self::new(adapters)
    }

    pub(crate) async fn create(
        &self,
        request: ProtocolCommandRequest,
        services: &ProtocolServices,
    ) -> Result<Box<dyn Command>> {
        let adapter = self
            .adapters
            .iter()
            .find(|adapter| adapter.supports(&request))
            .ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "No protocol adapter supports {}",
                    request.first_uri
                )))
            })?;

        adapter.create(request, services).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::command::CommandStatus;
    use crate::request::request_group::GroupId;
    use crate::util::rwlock_ext::RwLockRecover;

    struct FakeAdapter;

    #[async_trait]
    impl ProtocolCommandAdapter for FakeAdapter {
        fn supports(&self, request: &ProtocolCommandRequest) -> bool {
            request.first_uri.starts_with("fake:")
        }

        async fn create(
            &self,
            request: ProtocolCommandRequest,
            _services: &ProtocolServices,
        ) -> Result<Box<dyn Command>> {
            Ok(Box::new(FakeCommand {
                group: request.group,
            }))
        }
    }

    struct FakeCommand {
        group: Arc<std::sync::RwLock<RequestGroup>>,
    }

    #[async_trait]
    impl Command for FakeCommand {
        async fn execute(&mut self) -> Result<()> {
            Ok(())
        }

        fn status(&self) -> CommandStatus {
            CommandStatus::Pending
        }

        fn gid(&self) -> crate::request::request_group::GroupId {
            self.group.recover().gid()
        }
    }

    #[tokio::test]
    async fn newly_registered_adapter_is_selected_without_dispatcher_changes() {
        let registry = ProtocolAdapterRegistry::new(vec![Box::new(FakeAdapter)]);
        let options = Arc::new(DownloadOptions::default());
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(730),
            vec!["fake://host/object".to_string()],
            (*options).clone(),
        )));
        let request = ProtocolCommandRequest {
            group,
            first_uri: "fake://host/object".to_string(),
            options,
        };
        let services = ProtocolServices {
            dns_cache: Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            global_limiter: None,
        };

        let command = registry
            .create(request, &services)
            .await
            .expect("the fake adapter should be selected");

        assert_eq!(command.status(), CommandStatus::Pending);
        assert_eq!(command.gid(), GroupId::new(730));
    }
}
