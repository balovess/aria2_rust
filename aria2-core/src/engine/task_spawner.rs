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
use crate::ftp::FtpConnectionPool;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::rwlock_ext::RwLockRecover;

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
pub async fn spawn_download_task(
    group: Arc<std::sync::RwLock<RequestGroup>>,
    _ftp_pool: Arc<FtpConnectionPool>,
    _dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,
    global_limiter: Option<RateLimiter>,
    generation: u64,
    completion_tx: tokio::sync::mpsc::UnboundedSender<(GroupId, u64, TaskResult)>,
) -> Option<(
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
)> {
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

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

    // Metalink owns mirror ordering and torrent fallback. Dispatch it before
    // resolving the first mirror: one unavailable mirror must not prevent the
    // command from trying the remaining mirrors or the torrent fallback.
    #[cfg(feature = "metalink")]
    if let Some((metalink_data, file_index)) = group.recover().metalink_source() {
        let mut cmd = crate::engine::metalink_download_command::MetalinkDownloadCommand::new_with_group_source(
            Arc::clone(&group), &metalink_data, file_index, &options,
        );
        if let Ok(ref mut command) = cmd
            && let Some(limiter) = global_limiter.clone()
        {
            command.set_global_limiter(limiter);
        }
        let mut cmd: Box<dyn Command> = match cmd {
            Ok(command) => Box::new(command),
            Err(error) => {
                warn!(gid = gid.value(), error = %error, "Failed to create Metalink command");
                group.recover().dec_commands();
                return None;
            }
        };
        let completion_tx = completion_tx.clone();
        let handle = tokio::spawn(async move {
            let task_result = match tokio::select! {
                result = cmd.execute() => result,
                _ = &mut shutdown_rx => {
                    cmd.shutdown().await;
                    Err(Aria2Error::DownloadFailed("download shutdown requested".into()))
                }
            } {
                Ok(()) => TaskResult::Success,
                Err(error) => TaskResult::Failed(error),
            };
            let _ = completion_tx.send((gid, generation, task_result));
        });
        return Some((handle, shutdown_tx));
    }

    // Resolve the origin before constructing the command. This keeps DNS failures
    // on the same structured completion path as other command failures and lets
    // the engine apply NameResolveError consistently.
    if let Some((hostname, port)) = direct_origin(&first_uri) {
        let dns_result = {
            let mut cache = _dns_cache.lock().await;
            let result = cache.resolve(&hostname, port).await;
            drop(cache);
            result
        };
        if let Err(error) = dns_result {
            let protocol = url::Url::parse(&first_uri)
                .map(|url| url.scheme().to_string())
                .unwrap_or_default();
            let stat_man = ServerStatMan::shared();
            stat_man.get_or_create_with_protocol(&hostname, &protocol);
            stat_man.mark_failure_with_protocol(&hostname, &protocol, 0);
            warn!(gid = gid.value(), error = %error, "DNS resolution failed before command start");
            let completion_tx = completion_tx.clone();
            let handle = tokio::spawn(async move {
                let _ = completion_tx.send((gid, generation, TaskResult::Failed(error)));
            });
            return Some((handle, shutdown_tx));
        }
    }

    // Create the appropriate command based on URI scheme.
    let cmd_result = create_command_for_uri(
        &first_uri,
        Arc::clone(&group),
        &options,
        global_limiter,
        &_dns_cache,
    )
    .await;

    let mut cmd: Box<dyn Command> = match cmd_result {
        Ok(c) => c,
        Err(e) => {
            warn!(gid = gid.value(), error = %e, "Failed to create command for URI");
            group.recover().dec_commands();
            return None;
        }
    };

    debug!(gid = gid.value(), "Spawning download task for group");

    // Spawn the task. It sends completion via channel when done.
    let handle = tokio::spawn(async move {
        let result = tokio::select! {
            result = cmd.execute() => result,
            _ = &mut shutdown_rx => {
                cmd.shutdown().await;
                Err(Aria2Error::DownloadFailed("download shutdown requested".into()))
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
                TaskResult::Failed(Aria2Error::Recoverable(recoverable))
            }
            Err(e) => {
                warn!(gid = gid.value(), error = %e, "Download task failed");
                TaskResult::Failed(e)
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

    Some((handle, shutdown_tx))
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

/// Create the appropriate `Command` implementation for a URI.
///
/// Uses `new_with_group` constructors so the externally-managed `RequestGroup`
/// is preserved rather than creating a new one internally. This is critical
/// for the engine loop's `num_commands` tracking and promotion/demotion flow.
async fn create_command_for_uri(
    uri: &str,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    options: &DownloadOptions,
    global_limiter: Option<RateLimiter>,
    dns_cache: &Arc<tokio::sync::Mutex<DnsCache>>,
) -> crate::error::Result<Box<dyn Command>> {
    let uri_lower = uri.to_lowercase();

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
        if let Some(limiter) = global_limiter {
            cmd.set_global_limiter(limiter);
        }
        return Ok(Box::new(cmd));
    }

    // BitTorrent torrent payloads. The group must already have a resolved
    // DownloadContext, which is installed by BtDependency before promotion.
    #[cfg(feature = "bittorrent")]
    if uri_lower.starts_with("bt://") {
        let output_dir = options.dir.as_deref();
        let torrent_bytes = group
            .recover()
            .bt_metadata_data()
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
        if let Some(limiter) = global_limiter {
            cmd.set_global_limiter(limiter);
        }
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
        if let Some(limiter) = global_limiter.clone() {
            cmd.set_global_limiter(limiter);
        }
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
        if let Some(limiter) = global_limiter.clone() {
            cmd.set_global_limiter(limiter);
        }
        cmd.set_dns_cache(Arc::clone(dns_cache));
        if let Some((hostname, port)) = direct_origin(uri)
            && let Ok(addresses) = dns_cache.lock().await.resolve(&hostname, port).await
        {
            cmd.set_resolved_addresses(addresses);
        }
        return Ok(Box::new(cmd));
    }

    // Default: HTTP/HTTPS download command.
    let output_dir = options.dir.as_deref();
    let group_output_name = group.recover().output_name();
    let output_name = group_output_name.as_deref().or(options.out.as_deref());
    let resolved_addresses = if let Some((hostname, port)) = direct_origin(uri) {
        dns_cache.lock().await.resolve(&hostname, port).await.ok()
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
    if let Some(limiter) = global_limiter {
        cmd.set_global_limiter(limiter);
    }
    Ok(Box::new(cmd))
}
