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

/// Shared engine services available to command adapters.
#[derive(Clone)]
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

pub(crate) fn is_sftp_uri(uri: &str) -> bool {
    uri.split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("sftp"))
}

#[cfg(not(feature = "sftp"))]
struct UnsupportedSftpCommandAdapter;

#[cfg(not(feature = "sftp"))]
#[async_trait]
impl ProtocolCommandAdapter for UnsupportedSftpCommandAdapter {
    fn supports(&self, request: &ProtocolCommandRequest) -> bool {
        is_sftp_uri(&request.first_uri)
    }

    async fn create(
        &self,
        _request: ProtocolCommandRequest,
        _services: &ProtocolServices,
    ) -> Result<Box<dyn Command>> {
        Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol {
            protocol: "sftp".into(),
        }))
    }
}

fn sftp_command_adapter() -> Box<dyn ProtocolCommandAdapter> {
    #[cfg(feature = "sftp")]
    {
        Box::new(crate::engine::sftp::command_adapter::SftpCommandAdapter)
    }
    #[cfg(not(feature = "sftp"))]
    {
        Box::new(UnsupportedSftpCommandAdapter)
    }
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
        let ftp_connection_pool = Arc::clone(&_context.ftp_connection_pool);
        #[cfg(feature = "bittorrent")]
        let bittorrent = crate::engine::bittorrent::command_adapter::BtCommandServices {
            public_tracker_catalog: Arc::clone(&_context.public_tracker_catalog),
            bt_registry: Arc::clone(&_context.bt_registry),
            bt_listener: Arc::clone(&_context.bt_listener),
            lpd_manager: Arc::clone(&_context.lpd_manager),
            open_file_budget: Arc::clone(&_context.open_file_budget),
        };

        Self::builtins(
            ftp_connection_pool,
            #[cfg(feature = "bittorrent")]
            bittorrent,
        )
    }

    pub(crate) fn builtins(
        ftp_connection_pool: Arc<crate::ftp::connection_pool::FtpConnectionPool>,
        #[cfg(feature = "bittorrent")]
        bittorrent: crate::engine::bittorrent::command_adapter::BtCommandServices,
    ) -> Self {
        // Keep the always-available adapters in priority order; optional
        // formats are inserted ahead of their URI-scheme fallbacks below.
        #[allow(unused_mut)]
        let mut adapters: Vec<Box<dyn ProtocolCommandAdapter>> = vec![
            Box::new(crate::engine::ftp::command_adapter::FtpCommandAdapter::new(
                Arc::clone(&ftp_connection_pool),
            )),
            Box::new(crate::engine::http::command_adapter::HttpCommandAdapter),
        ];

        #[cfg(feature = "metalink")]
        adapters.insert(
            0,
            Box::new(
                crate::engine::metalink::command_adapter::MetalinkCommandAdapter::new(
                    Arc::clone(&ftp_connection_pool),
                    #[cfg(feature = "bittorrent")]
                    bittorrent.clone(),
                ),
            ),
        );

        adapters.insert(
            usize::from(cfg!(feature = "metalink")),
            sftp_command_adapter(),
        );

        #[cfg(feature = "bittorrent")]
        adapters.insert(
            usize::from(cfg!(feature = "metalink")) + usize::from(cfg!(feature = "sftp")),
            Box::new(crate::engine::bittorrent::command_adapter::BtCommandAdapter::new(bittorrent)),
        );

        Self::new(adapters)
    }

    /// Build only adapters that can fetch a single Metalink mirror or
    /// metadata URL. Metadata dispatch is owned by Metalink, so this registry
    /// deliberately excludes Metalink itself and BitTorrent task commands.
    #[cfg(feature = "metalink")]
    pub(crate) fn source_protocols(
        ftp_connection_pool: Arc<crate::ftp::connection_pool::FtpConnectionPool>,
    ) -> Self {
        #[allow(unused_mut)]
        let mut adapters: Vec<Box<dyn ProtocolCommandAdapter>> = vec![
            Box::new(crate::engine::ftp::command_adapter::FtpCommandAdapter::new(
                ftp_connection_pool,
            )),
            Box::new(crate::engine::http::command_adapter::HttpCommandAdapter),
        ];

        adapters.insert(1, sftp_command_adapter());

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

    #[test]
    fn sftp_scheme_match_is_case_insensitive_and_requires_a_scheme_boundary() {
        assert!(is_sftp_uri("sftp://host/file"));
        assert!(is_sftp_uri("SFTP://host/file"));
        assert!(is_sftp_uri("sftp:opaque"));
        assert!(!is_sftp_uri("sftpish://host/file"));
        assert!(!is_sftp_uri("https://host/sftp://file"));
    }

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

    #[cfg(all(feature = "metalink", not(feature = "bittorrent")))]
    #[tokio::test]
    async fn metalink_adapter_uses_parsed_file_before_session_source_fallback() {
        let options = Arc::new(DownloadOptions::default());
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(731),
            vec!["https://mirror.test/payload.bin".to_string()],
            (*options).clone(),
        )));
        group
            .recover()
            .set_metalink_source(b"not valid XML".to_vec(), 0);
        let document = aria2_protocol::metalink::parser::MetalinkDocument::parse(
            br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://mirror.test/payload.bin</url></file></metalink>"#,
            None,
        )
        .expect("fixture should parse");
        group
            .recover()
            .set_parsed_metalink_file(Arc::new(document.files.into_iter().next().unwrap()));

        let registry = ProtocolAdapterRegistry::new(vec![Box::new(
            crate::engine::metalink::command_adapter::MetalinkCommandAdapter::new(Arc::new(
                crate::ftp::connection_pool::FtpConnectionPool::new(4),
            )),
        )]);
        let request = ProtocolCommandRequest {
            group,
            first_uri: "https://mirror.test/payload.bin".to_string(),
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
            .expect("cached protocol metadata should avoid reparsing the invalid source bytes");

        assert_eq!(command.gid(), GroupId::new(731));
    }

    #[cfg(all(feature = "metalink", not(feature = "bittorrent")))]
    #[tokio::test]
    async fn metalink_adapter_reconstructs_parsed_file_from_restored_source() {
        let options = Arc::new(DownloadOptions::default());
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(732),
            vec!["https://mirror.test/restored.bin".to_string()],
            (*options).clone(),
        )));
        group.recover().set_metalink_source(
            br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="restored.bin"><url>https://mirror.test/restored.bin</url></file></metalink>"#.to_vec(),
            0,
        );
        let request = ProtocolCommandRequest {
            group,
            first_uri: "https://mirror.test/restored.bin".to_string(),
            options,
        };
        let registry = ProtocolAdapterRegistry::new(vec![Box::new(
            crate::engine::metalink::command_adapter::MetalinkCommandAdapter::new(Arc::new(
                crate::ftp::connection_pool::FtpConnectionPool::new(4),
            )),
        )]);
        let services = ProtocolServices {
            dns_cache: Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            global_limiter: None,
        };

        let command = registry
            .create(request, &services)
            .await
            .expect("restored source should be parsed into command input");

        assert_eq!(command.gid(), GroupId::new(732));
    }

    #[cfg(feature = "metalink")]
    #[tokio::test]
    async fn metalink_payload_uses_injected_http_proxy_policy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("HTTP proxy fixture should bind");
        let proxy_address = listener.local_addr().expect("HTTP proxy address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("HTTP proxy request");
            let mut request = [0u8; 4096];
            let length = stream.read(&mut request).await.expect("read proxy request");
            let request = String::from_utf8_lossy(&request[..length]).into_owned();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload",
                )
                .await
                .expect("write HTTP payload");
            request
        });

        let output_dir = tempfile::tempdir_in("../target").expect("output directory");
        let xml = "<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"payload.bin\"><size>7</size><url>http://mirror.test/payload.bin</url></file></metalink>".to_string();
        let options = Arc::new(DownloadOptions {
            dir: Some(output_dir.path().to_string_lossy().into_owned()),
            http_proxy: Some(format!("http://{proxy_address}")),
            max_retries: 1,
            ..DownloadOptions::default()
        });
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(733),
            vec!["http://mirror.test/payload.bin".to_string()],
            (*options).clone(),
        )));
        group
            .recover()
            .set_metalink_source(xml.as_bytes().to_vec(), 0);
        let document =
            aria2_protocol::metalink::parser::MetalinkDocument::parse(xml.as_bytes(), None)
                .expect("fixture should parse");
        group
            .recover()
            .set_parsed_metalink_file(Arc::new(document.files.into_iter().next().unwrap()));

        #[cfg(feature = "bittorrent")]
        let adapter = crate::engine::metalink::command_adapter::MetalinkCommandAdapter::new(
            Arc::new(crate::ftp::connection_pool::FtpConnectionPool::new(4)),
            crate::engine::bittorrent::command_adapter::BtCommandServices {
                public_tracker_catalog: Arc::new(
                    aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new(),
                ),
                bt_registry: Arc::new(std::sync::RwLock::new(
                    crate::engine::bittorrent::registry::BtRegistry::new(),
                )),
                bt_listener: Arc::new(
                    crate::engine::bittorrent::peer::listener::BtPeerListenerManager::new(),
                ),
                lpd_manager: Arc::new(crate::engine::bittorrent::discovery::lpd::LpdManager::new()),
                open_file_budget: crate::filesystem::open_file_budget::OpenFileBudget::new(20),
            },
        );
        #[cfg(not(feature = "bittorrent"))]
        let adapter = crate::engine::metalink::command_adapter::MetalinkCommandAdapter::new(
            Arc::new(crate::ftp::connection_pool::FtpConnectionPool::new(4)),
        );
        let adapter = ProtocolAdapterRegistry::new(vec![Box::new(adapter)]);
        let services = ProtocolServices {
            dns_cache: Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            global_limiter: None,
        };
        let mut command = adapter
            .create(
                ProtocolCommandRequest {
                    group,
                    first_uri: "http://mirror.test/payload.bin".to_string(),
                    options,
                },
                &services,
            )
            .await
            .expect("Metalink command should be created");

        command.execute().await.expect("Metalink should complete");
        assert_eq!(
            tokio::fs::read(output_dir.path().join("payload.bin"))
                .await
                .unwrap(),
            b"payload"
        );
        let request = server.await.expect("HTTP proxy fixture should finish");
        assert!(request.starts_with("GET http://mirror.test/payload.bin HTTP/1.1"));
    }
}
