//! Task spawner: creates tokio tasks from promoted download groups.
//!
//! When the engine promotes a group from reserved to active, it needs to
//! create the appropriate `Command` implementation (DownloadCommand,
//! BtDownloadCommand, etc.) and spawn it as a tokio task. This module
//! handles that dispatch, wiring up the completion channel so the engine
//! can track when tasks finish.

use std::sync::Arc;
use tracing::{debug, warn};

use super::command::Command;
use super::engine_command::TaskResult;
use super::protocol_adapter::{ProtocolAdapterRegistry, ProtocolCommandRequest, ProtocolServices};
use crate::error::Aria2Error;
use crate::network::ConnectionContext;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;
use tokio_util::sync::CancellationToken;

/// Shared services required while constructing a command.
pub(crate) struct CommandDependencies {
    pub(crate) services: ProtocolServices,
    pub(crate) protocol_adapters: Arc<ProtocolAdapterRegistry>,
}

/// Spawns a download command as a tokio task and wires up the completion
/// channel. Returns the `JoinHandle` for task management.
///
/// The command is created based on the group's URIs and options:
/// - BitTorrent magnet URIs → BtDownloadCommand
/// - FTP URIs (`ftp://`, `ftps://`) → FtpDownloadCommand
/// - HTTP/HTTPS → DownloadCommand
///
/// Before spawning, increments `RequestGroup::num_commands`
/// (mirrors C++ AbstractCommand constructor).
///
/// After the task completes, sends `(GID, generation, TaskResult)` via the completion
/// channel so the engine can decrement `num_commands` and check for demotion.
pub(crate) fn spawn_download_task(
    group: Arc<std::sync::RwLock<RequestGroup>>,
    dependencies: CommandDependencies,
    generation: u64,
    completion_tx: tokio::sync::mpsc::UnboundedSender<(GroupId, u64, TaskResult)>,
) -> Option<(tokio::task::JoinHandle<()>, CancellationToken)> {
    let gid = group.recover().gid();
    let uris = group.recover().uris().to_vec();
    let options = group.recover().options_arc();

    // Increment command counter BEFORE spawning.
    group.recover().inc_commands();

    // Determine the first URI to decide which command type to create.
    let first_uri = match uris.first() {
        Some(u) => u.clone(),
        None => {
            warn!(gid = gid.value(), "No URIs in group, cannot spawn task");
            group.recover().dec_commands();
            return None;
        }
    };

    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let completion_tx = completion_tx.clone();

    // Command construction may perform DNS resolution and build protocol
    // clients. Keep that work in the tracked task so the single-threaded
    // engine loop can continue processing pause/remove commands while a
    // resolver or a slow protocol constructor is waiting.
    let handle = tokio::spawn(async move {
        let (result, connection_context) = tokio::select! {
            command_result = create_command_for_group(
                Arc::clone(&group),
                first_uri.to_string(),
                options,
                dependencies,
            ) => {
                match command_result {
                    Ok(mut cmd) => {
                        // Once a command has been constructed, its protocol
                        // loop owns lifecycle cleanup. Force-halt and remove
                        // already update the RequestGroup, so dropping the
                        // execute future here would bypass writer flushing and
                        // protocol-specific checkpoints.
                        let result = cmd.execute().await;
                        if result.is_err() {
                            cmd.shutdown().await;
                        }
                        (result, cmd.connection_context())
                    }
                    Err(error) => (Err(error), None),
                }
            }
            _ = task_shutdown.cancelled() => {
                (Err(Aria2Error::DownloadFailed("download shutdown requested".into())), None)
            }
        };
        let task_result = match result {
            Ok(()) => {
                debug!(gid = gid.value(), "Download task completed successfully");
                TaskResult::Success
            }
            Err(Aria2Error::Recoverable(recoverable)) => {
                warn!(
                    gid = gid.value(),
                    "Download task failed with recoverable error"
                );
                failed_task_result(Aria2Error::Recoverable(recoverable), connection_context)
            }
            Err(e) => {
                warn!(gid = gid.value(), error = %e, "Download task failed");
                failed_task_result(e, connection_context)
            }
        };

        // Send completion notification. If the channel is closed, the engine
        // has already shut down; just log and move on.
        if completion_tx.send((gid, generation, task_result)).is_err() {
            debug!(
                gid = gid.value(),
                "Completion channel closed, engine likely shut down"
            );
        }
    });

    Some((handle, shutdown))
}

fn failed_task_result(
    error: Aria2Error,
    connection_context: Option<ConnectionContext>,
) -> TaskResult {
    match connection_context {
        Some(connection_context) => TaskResult::FailedWithContext {
            error,
            connection_context,
        },
        None => TaskResult::Failed(error),
    }
}

/// Build the protocol command inside the tracked task.
///
/// The registry gives metadata adapters and URI adapters the same construction
/// seam. Keeping construction inside the tracked task ensures DNS and client
/// setup remain cancellable without blocking the engine loop.
async fn create_command_for_group(
    group: Arc<std::sync::RwLock<RequestGroup>>,
    first_uri: String,
    options: Arc<DownloadOptions>,
    dependencies: CommandDependencies,
) -> crate::error::Result<Box<dyn Command>> {
    create_command_for_uri(&first_uri, group, &options, dependencies).await
}

/// Select a registered adapter and preserve the engine-owned `RequestGroup`.
async fn create_command_for_uri(
    uri: &str,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    options: &DownloadOptions,
    dependencies: CommandDependencies,
) -> crate::error::Result<Box<dyn Command>> {
    dependencies
        .protocol_adapters
        .create(
            ProtocolCommandRequest {
                group,
                first_uri: uri.to_string(),
                options: Arc::new(options.clone()),
            },
            &dependencies.services,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::dns_cache::DnsCache;
    use crate::network::OutboundNetworkPolicy;
    use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

    fn dependencies(dns_cache: Arc<tokio::sync::Mutex<DnsCache>>) -> CommandDependencies {
        CommandDependencies {
            services: ProtocolServices {
                dns_cache,
                outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
                global_limiter: None,
            },
            protocol_adapters: Arc::new(ProtocolAdapterRegistry::builtins(
                #[cfg(feature = "bittorrent")]
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
                    lpd_manager: Arc::new(
                        crate::engine::bittorrent::discovery::lpd::LpdManager::new(),
                    ),
                },
            )),
        }
    }

    #[tokio::test]
    async fn async_dns_false_uses_the_protocol_default_resolver_path() {
        for scheme in ["http", "ftp"] {
            let cache = Arc::new(tokio::sync::Mutex::new(DnsCache::new()));
            let options = DownloadOptions {
                async_dns: false,
                ..DownloadOptions::default()
            };
            let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                GroupId::new(70),
                vec![format!("{scheme}://localhost/file.bin")],
                options.clone(),
            )));

            let _command = create_command_for_uri(
                &format!("{scheme}://localhost/file.bin"),
                group,
                &options,
                dependencies(Arc::clone(&cache)),
            )
            .await
            .expect("command construction should not require the shared DNS cache");

            assert_eq!(
                cache.lock().await.len(),
                0,
                "async-dns=false must not pre-resolve {scheme} through the shared cache"
            );
        }
    }

    #[tokio::test]
    async fn async_dns_true_populates_the_shared_cache_for_protocol_commands() {
        for scheme in ["http", "ftp"] {
            let cache = Arc::new(tokio::sync::Mutex::new(DnsCache::new()));
            let options = DownloadOptions::default();
            let uri = format!("{scheme}://localhost/file.bin");
            let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                GroupId::new(71),
                vec![uri.clone()],
                options.clone(),
            )));

            let _command =
                create_command_for_uri(&uri, group, &options, dependencies(Arc::clone(&cache)))
                    .await
                    .expect("command construction should resolve localhost");

            assert_eq!(
                cache.lock().await.len(),
                1,
                "async-dns=true must use the shared cache for {scheme}"
            );
        }
    }
}
