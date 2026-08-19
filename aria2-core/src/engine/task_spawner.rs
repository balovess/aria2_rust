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
use crate::dns::dns_cache::DnsCache;
use crate::error::Aria2Error;
use crate::network::ConnectionContext;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;
use tokio_util::sync::CancellationToken;

/// Shared services required while constructing a command.
pub(crate) struct CommandDependencies {
    pub(crate) dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,
    pub(crate) global_limiter: Option<RateLimiter>,
    #[cfg(feature = "bittorrent")]
    pub(crate) public_tracker_catalog:
        Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>,
    #[cfg(feature = "bittorrent")]
    pub(crate) bt_registry: Arc<std::sync::RwLock<crate::engine::bt_registry::BtRegistry>>,
    #[cfg(feature = "bittorrent")]
    pub(crate) bt_listener: Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>,
    #[cfg(feature = "bittorrent")]
    pub(crate) lpd_manager: Arc<crate::engine::lpd_manager::LpdManager>,
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
    completion_tx: tokio::sync::mpsc::Sender<(GroupId, u64, TaskResult)>,
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
                first_uri,
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
        if completion_tx
            .send((gid, generation, task_result))
            .await
            .is_err()
        {
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

fn direct_origin(uri: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(uri).ok()?;
    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https" | "ftp" | "ftps") {
        return None;
    }
    Some((
        parsed.host_str()?.to_string(),
        parsed.port_or_known_default()?,
    ))
}

/// Build the protocol command inside the tracked task.
///
/// Metalink commands need their source metadata before URI dispatch, while
/// the other protocols share the regular scheme factory. Keeping both paths
/// here ensures command construction remains cancellable and never blocks the
/// engine loop on DNS or client setup.
async fn create_command_for_group(
    group: Arc<std::sync::RwLock<RequestGroup>>,
    first_uri: String,
    options: Arc<DownloadOptions>,
    dependencies: CommandDependencies,
) -> crate::error::Result<Box<dyn Command>> {
    #[cfg(feature = "metalink")]
    if let Some((metalink_data, file_index)) = group.recover().metalink_source() {
        let base_uri = group.recover().metalink_base_uri();
        let mut command = crate::engine::metalink_download_command::MetalinkDownloadCommand::new_with_group_source(
            Arc::clone(&group),
            &metalink_data,
            file_index,
            &options,
            base_uri.as_deref(),
        )?;
        if let Some(limiter) = dependencies.global_limiter.clone() {
            command.set_global_limiter(limiter);
        }
        #[cfg(feature = "bittorrent")]
        command.set_public_tracker_catalog(Arc::clone(&dependencies.public_tracker_catalog));
        #[cfg(feature = "bittorrent")]
        {
            command.set_bt_registry(Arc::clone(&dependencies.bt_registry));
            command.set_bt_listener(Arc::clone(&dependencies.bt_listener));
            command.set_lpd_manager(Arc::clone(&dependencies.lpd_manager));
        }
        return Ok(Box::new(command));
    }

    create_command_for_uri(&first_uri, group, &options, dependencies).await
}

/// Create the appropriate `Command` implementation for a URI.
///
/// Uses `new_with_group` constructors so the externally-managed `RequestGroup`
/// is preserved rather than creating a new one internally. This is critical
/// for the engine loop's `num_commands` tracking and promotion/demotion flow.
async fn create_command_for_uri(
    uri: &str,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    options: &DownloadOptions,
    dependencies: CommandDependencies,
) -> crate::error::Result<Box<dyn Command>> {
    let dns_cache = &dependencies.dns_cache;
    let use_async_dns = options.async_dns;
    let uri_lower = uri.to_lowercase();
    #[cfg(feature = "bittorrent")]
    let bt_metadata = group.recover().bt_metadata_data();

    // SFTP downloads use the engine-owned group, matching other v2 protocols.
    #[cfg(feature = "sftp")]
    if uri_lower.starts_with("sftp://") {
        let mut cmd = crate::engine::sftp_download_command::SftpDownloadCommand::new_with_group(
            Arc::clone(&group),
            uri,
            options,
            options.dir.as_deref(),
            options.out.as_deref(),
        )?;
        if let Some(limiter) = dependencies.global_limiter.clone() {
            cmd.set_global_limiter(limiter);
        }
        return Ok(Box::new(cmd));
    }

    // BitTorrent torrent payloads. Followed torrent groups retain tracker and
    // web-seed URIs, so their first URI is not necessarily `bt://`.
    #[cfg(feature = "bittorrent")]
    if uri_lower.starts_with("bt://") || bt_metadata.is_some() {
        let output_dir = options.dir.as_deref();
        let torrent_bytes = bt_metadata
            .or_else(|| {
                group
                    .recover()
                    .metadata_info()
                    .and_then(|info| info.metadata_path().map(std::path::PathBuf::from))
                    .and_then(|path| std::fs::read(path).ok())
            })
            .ok_or_else(|| {
                Aria2Error::Fatal(crate::error::FatalError::Config(
                    "Resolved BitTorrent payload has no metadata source".to_string(),
                ))
            })?;
        let mut cmd = crate::engine::bt_download_command::BtDownloadCommand::new_with_group(
            group,
            &torrent_bytes,
            options,
            output_dir,
        )?;
        cmd.set_bt_listener(Arc::clone(&dependencies.bt_listener));
        cmd.set_bt_registry(Arc::clone(&dependencies.bt_registry));
        cmd.set_lpd_manager(Arc::clone(&dependencies.lpd_manager));
        if let Some(limiter) = dependencies.global_limiter.clone() {
            cmd.set_global_limiter(limiter);
        }
        cmd.set_public_tracker_catalog(Arc::clone(&dependencies.public_tracker_catalog));
        return Ok(Box::new(cmd));
    }

    // BitTorrent magnet links.
    #[cfg(feature = "bittorrent")]
    if uri_lower.starts_with("magnet:") {
        let output_dir = options.dir.as_deref();
        let mut cmd =
            crate::engine::magnet_download_command::MagnetDownloadCommand::new_with_group(
                group, output_dir,
            )?;
        cmd.set_bt_listener(Arc::clone(&dependencies.bt_listener));
        cmd.set_bt_registry(Arc::clone(&dependencies.bt_registry));
        cmd.set_lpd_manager(Arc::clone(&dependencies.lpd_manager));
        if let Some(limiter) = dependencies.global_limiter.clone() {
            cmd.set_global_limiter(limiter);
        }
        cmd.set_public_tracker_catalog(Arc::clone(&dependencies.public_tracker_catalog));
        return Ok(Box::new(cmd));
    }

    // FTP/FTPS downloads.
    if uri_lower.starts_with("ftp://") || uri_lower.starts_with("ftps://") {
        let output_dir = options.dir.as_deref();
        let output_name = options.out.as_deref();
        let mut cmd = crate::engine::ftp_download_command::FtpDownloadCommand::new_with_group(
            group,
            output_dir,
            output_name,
        )?;
        if let Some(limiter) = dependencies.global_limiter.clone() {
            cmd.set_global_limiter(limiter);
        }
        if use_async_dns {
            cmd.set_dns_cache(Arc::clone(dns_cache));
            if let Some((hostname, port)) = direct_origin(uri)
                && let Ok(addresses) = dns_cache
                    .lock()
                    .await
                    .resolve_with_refresh(&hostname, port)
                    .await
            {
                cmd.set_resolved_addresses(addresses);
            }
        }
        return Ok(Box::new(cmd));
    }

    // Default: HTTP/HTTPS download command.
    let output_dir = options.dir.as_deref();
    let group_output_name = group.recover().output_name();
    let output_name = group_output_name.as_deref().or(options.out.as_deref());
    let resolved_addresses = if use_async_dns {
        if let Some((hostname, port)) = direct_origin(uri) {
            dns_cache
                .lock()
                .await
                .resolve_with_refresh(&hostname, port)
                .await
                .ok()
        } else {
            None
        }
    } else {
        None
    };
    let mut cmd =
        crate::engine::download_command::DownloadCommand::new_with_group_and_resolved_addresses(
            group,
            uri,
            options,
            output_dir,
            output_name,
            resolved_addresses,
        )?;
    if let Some(limiter) = dependencies.global_limiter {
        cmd.set_global_limiter(limiter);
    }
    Ok(Box::new(cmd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

    fn dependencies(
        dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,
    ) -> CommandDependencies {
        CommandDependencies {
            dns_cache,
            global_limiter: None,
            #[cfg(feature = "bittorrent")]
            public_tracker_catalog: Arc::new(
                aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new(),
            ),
            #[cfg(feature = "bittorrent")]
            bt_registry: Arc::new(std::sync::RwLock::new(
                crate::engine::bt_registry::BtRegistry::new(),
            )),
            #[cfg(feature = "bittorrent")]
            bt_listener: Arc::new(crate::engine::bt_peer_listener::BtPeerListenerManager::new()),
            #[cfg(feature = "bittorrent")]
            lpd_manager: Arc::new(crate::engine::lpd_manager::LpdManager::new()),
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

            let _command = create_command_for_uri(
                &uri,
                group,
                &options,
                dependencies(Arc::clone(&cache)),
            )
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
