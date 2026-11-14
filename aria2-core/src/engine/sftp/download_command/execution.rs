//! SFTP download execution logic.
//!
//! Executes one SFTP attempt: SSH connect, SFTP session init, remote file
//! inspection, chunked reads, and cleanup/finalize. Retry and command lifecycle
//! behavior lives in `command.rs`.

use std::sync::Arc;
use std::time::Instant;
use std::{future::Future, time::Duration};
use tokio::net::TcpStream;

use tracing::{debug, error, info, warn};

use crate::checksum::checksum::Checksum;
use crate::constants;
use crate::dns::dns_cache::DnsCache;
use crate::engine::work_commit::SequentialWorkCommitter;
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_writer::{DiskWriter, new_sequential_download_writer};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use aria2_protocol::sftp::connection::{SshConnection, SshError};
use aria2_protocol::sftp::file_ops::{OpenFlags, SftpFileOps};
use aria2_protocol::sftp::session::SftpSession;

use super::types::SftpDownloadCommand;

impl SftpDownloadCommand {
    async fn await_with_lifecycle<T, F>(&self, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let notifier = self.group.recover().lifecycle_notifier();
        tokio::pin!(future);
        loop {
            let notified = notifier.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            self.check_cancelled()?;

            tokio::select! {
                biased;
                _ = &mut notified => self.check_cancelled()?,
                result = &mut future => return result,
            }
        }
    }

    pub(super) fn check_cancelled(&self) -> Result<()> {
        let group = self.group.recover();
        if group.is_removed() {
            Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            ))
        } else if group.is_paused_flag() {
            Err(Aria2Error::DownloadFailed("Download paused".into()))
        } else if group.is_force_halt_requested() || group.is_halt_requested() {
            Err(Aria2Error::DownloadFailed("SFTP download halted".into()))
        } else {
            Ok(())
        }
    }

    /// Execute the SFTP download.
    ///
    /// This is the main entry point called by the download engine. It orchestrates
    /// the entire download lifecycle from connection establishment through data
    /// transfer to cleanup.
    pub(super) async fn execute_once(&mut self) -> Result<()> {
        self.check_cancelled()?;
        let connection_guard = ActiveConnectionGuard::new(Arc::clone(&self.group));
        connection_guard.set(1);
        // -----------------------------------------------------------------
        // Phase 0: Initialization
        // -----------------------------------------------------------------
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        let in_memory_download = self.group.recover().is_in_memory_download();

        debug!(
            "[SFTP-CMD] Starting download: {}@{}:{} -> {}",
            self.username,
            self.host,
            self.port,
            self.output_path.display()
        );

        // Ensure output directory exists
        if !in_memory_download
            && let Some(parent) = self.output_path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "Failed to create output directory: {}",
                    e
                )))
            })?;
        }

        // -----------------------------------------------------------------
        // Phase 1: SSH Connection
        // -----------------------------------------------------------------
        let ssh_options = self.build_ssh_options();
        let connect_timeout = ssh_options.connect_timeout;
        let host = self.host.as_str();
        let port = self.port;
        let outbound_network_policy = Arc::clone(&self.outbound_network_policy);
        let dns_cache = self.dns_cache.clone();
        let host_for_error = self.host.clone();
        let remote_path_for_error = self.remote_path.clone();
        let conn_result = self
            .await_with_lifecycle(async move {
                connect_with_timeout(host, port, connect_timeout, async move {
                    let stream = connect_sftp_host(
                        host,
                        port,
                        dns_cache.as_deref(),
                        &outbound_network_policy,
                    )
                    .await?;
                    SshConnection::connect_with_stream(ssh_options, stream).await
                })
                .await
                .map_err(|error| {
                    Self::map_ssh_error(&error, &host_for_error, port, &remote_path_for_error)
                })
            })
            .await;

        let mut conn = match conn_result {
            Ok(c) => c,
            Err(error) => return Err(error),
        };

        info!(
            "[SFTP-CMD] SSH connected: {}@{}:{}",
            self.username, self.host, self.port
        );

        // -----------------------------------------------------------------
        // Phase 2: SFTP Session Initialization
        // -----------------------------------------------------------------
        let session_result = self
            .await_with_lifecycle(async {
                SftpSession::open(&mut conn)
                    .await
                    .map_err(|error| Self::map_session_error(&error))
            })
            .await;

        let session: SftpSession = match session_result {
            Ok(s) => s,
            Err(error) => {
                // Attempt graceful disconnect before returning error
                let _ = conn.disconnect().await;
                return Err(error);
            }
        };

        debug!(
            "[SFTP-CMD] SFTP session established (v{})",
            session.server_version()
        );

        // -----------------------------------------------------------------
        // Phase 3: Stat Remote File
        // -----------------------------------------------------------------
        let ops = SftpFileOps::new(&session);

        let file_attrs = match self
            .await_with_lifecycle(async {
                ops.stat(&self.remote_path)
                    .await
                    .map_err(|error| Self::map_file_op_error(&error, &self.host, &self.remote_path))
            })
            .await
        {
            Ok(attrs) => attrs,
            Err(error) => {
                let _ = conn.disconnect().await;
                return Err(error);
            }
        };

        if !file_attrs.is_regular_file {
            let _ = conn.disconnect().await;
            return Err(Aria2Error::Fatal(FatalError::FileNotFound {
                path: format!("{} (not a regular file)", self.remote_path),
            }));
        }

        let total_length = file_attrs.size;
        info!(
            "[SFTP-CMD] Remote file size: {} bytes ({:.2} MB)",
            total_length,
            total_length as f64 / (1024.0 * 1024.0)
        );

        // Update RequestGroup with total length and recover a local prefix.
        // SFTP reads are positioned, so an existing partial file can be resumed
        // without downloading or rewriting its already-present prefix.
        let existing_length = if in_memory_download {
            0
        } else {
            tokio::fs::metadata(&self.output_path)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        };
        let continue_download = self.group.recover().options().continue_download;
        let checksum_config = self.group.recover().options().checksum.clone();
        let complete_local_checksum_candidate =
            !in_memory_download && existing_length == total_length && checksum_config.is_some();
        let resume_input_length = if complete_local_checksum_candidate {
            // aria2_original routes a complete local file with a configured
            // checksum through ChecksumCheckIntegrityEntry even without
            // `--continue`; only a failed check returns to remote download.
            existing_length
        } else {
            crate::engine::progress_checkpoint::ProgressCheckpoint::resume_input_length(
                &self.output_path,
                existing_length,
                continue_download,
                total_length,
            )
            .await
        };
        Self::validate_resume_offset(resume_input_length, total_length)?;
        if in_memory_download {
            self.checkpoint = None;
            self.completed_bytes = 0;
        } else {
            self.checkpoint = Some(
                crate::engine::progress_checkpoint::ProgressCheckpoint::open(
                    &self.output_path,
                    total_length,
                    resume_input_length,
                )
                .await,
            );
            self.completed_bytes = self
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.resume_offset(resume_input_length))
                .unwrap_or(resume_input_length);
        }
        {
            let g = self.group.recover();
            g.set_total_length(total_length);
            g.update_progress(self.completed_bytes);
        }

        if complete_local_checksum_candidate && self.completed_bytes == total_length {
            let (algorithm, expected) = checksum_config
                .as_ref()
                .expect("complete local checksum candidate has a checksum");
            let checksum = Checksum::from_type_and_value(algorithm, expected)?;
            if crate::checksum::check_integrity::man::enqueue_file_checksum_for_group(
                &crate::checksum::check_integrity::man::shared(),
                std::sync::Arc::clone(&self.group),
                &self.output_path,
                total_length,
                checksum,
            )
            .await?
            {
                self.group.recover().set_checksum_verified(true);
                self.group.recover().set_completed_length(total_length);
                self.group.recover_mut().complete()?;
                self.complete_checkpoint().await;
                info!(
                    path = %self.output_path.display(),
                    size = total_length,
                    "SFTP target already matches remote size and checksum"
                );
                let _ = conn.disconnect().await;
                return Ok(());
            }

            warn!(
                path = %self.output_path.display(),
                "SFTP target checksum mismatch; restarting from byte zero"
            );
            self.completed_bytes = 0;
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(0, true).await;
            }
        }

        // -----------------------------------------------------------------
        // Phase 4: Open Remote File for Reading
        // -----------------------------------------------------------------
        let mut remote_file = match self
            .await_with_lifecycle(async {
                ops.open(&self.remote_path, OpenFlags::readonly(), 0)
                    .await
                    .map_err(|error| Self::map_file_op_error(&error, &self.host, &self.remote_path))
            })
            .await
        {
            Ok(f) => f,
            Err(error) => {
                let _ = conn.disconnect().await;
                return Err(error);
            }
        };

        // -----------------------------------------------------------------
        // Phase 5: Prepare Local Disk Writer
        // -----------------------------------------------------------------
        let raw_writer = new_sequential_download_writer(
            &self.output_path,
            in_memory_download,
            self.completed_bytes,
            Some(total_length),
        );

        // Apply rate limiting if configured
        let rate_limit = {
            let g = self.group.recover();
            g.options().max_download_limit
        };
        // Global (process-wide) limiter: when present and enabled, the writer
        // acquires tokens after the per-download limiter so all concurrent
        // downloads share a single bandwidth ceiling.
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());
        let mut writer: Box<dyn DiskWriter> = if rate_limit.is_some() || global_limited {
            let per_rate = rate_limit.filter(|&r| r > 0);
            let limiter = per_rate
                .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)))
                .unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = self.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            raw_writer
        };

        // -----------------------------------------------------------------
        // Phase 6: Main Download Loop (Chunked Read + Write)
        // -----------------------------------------------------------------
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed: u64 = 0;
        let _buf = vec![0u8; constants::SFTP_DISK_WRITE_CHUNK_SIZE];

        info!("[SFTP-CMD] Starting transfer loop: {} bytes", total_length);

        loop {
            let halted = {
                let group = self.group.recover();
                group.is_removed() || group.is_force_halt_requested() || group.is_halt_requested()
            };
            if halted {
                let halt_error = {
                    let group = self.group.recover();
                    if group.is_removed() {
                        "Download cancelled by user"
                    } else if group.is_paused_flag() {
                        "Download paused"
                    } else {
                        "SFTP download halted"
                    }
                };
                let _ = remote_file.close().await;
                self.finalize_partial_writer(&mut writer).await;
                self.flush_checkpoint().await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::DownloadFailed(halt_error.into()));
            }

            let remaining = total_length.saturating_sub(self.completed_bytes);
            if remaining == 0 {
                break; // Download complete
            }

            // Calculate how much to read this iteration
            let to_read = (constants::SFTP_DISK_WRITE_CHUNK_SIZE as u64).min(remaining) as usize;

            // Read chunk from remote file at current offset
            let lifecycle_notify = self.group.recover().lifecycle_notifier();
            let lifecycle_changed = lifecycle_notify.notified();
            tokio::pin!(lifecycle_changed);
            lifecycle_changed.as_mut().enable();
            let data = tokio::select! {
                data = remote_file.read_at(self.completed_bytes, to_read as u32) => match data {
                Ok(data) if data.is_empty() => {
                    debug!(
                        "[SFTP-CMD] EOF at offset {} (expected {})",
                        self.completed_bytes, total_length
                    );
                    let _ = remote_file.close().await;
                    self.finalize_partial_writer(&mut writer).await;
                    let _ = conn.disconnect().await;
                    return Err(Aria2Error::DownloadFailed(format!(
                        "SFTP remote file ended before the advertised length: {} < {}",
                        self.completed_bytes, total_length
                    )));
                }
                Ok(data) => data,
                Err(e) => {
                    error!(
                        "[SFTP-CMD] Read error at offset {}: {}",
                        self.completed_bytes, e
                    );
                    let _ = remote_file.close().await;
                    self.finalize_partial_writer(&mut writer).await;
                    let _ = conn.disconnect().await;
                    return Err(Self::map_file_op_error(&e, &self.host, &self.remote_path));
                }
                },
                _ = &mut lifecycle_changed => {
                    if let Err(error) = self.check_cancelled() {
                        // CLOSE would be queued behind the stalled READ on
                        // the same SFTP session. Dropping the handle and SSH
                        // connection cancels the outstanding request without
                        // waiting for a protocol response.
                        drop(remote_file);
                        self.finalize_partial_writer(&mut writer).await;
                        self.flush_checkpoint().await;
                        let _ = conn.disconnect().await;
                        return Err(error);
                    }

                    // Save-session and other non-terminal lifecycle updates
                    // leave the remote read alive; retry the current offset.
                    continue;
                }
            };
            self.group.recover().record_network_activity();

            // Core work output commits source chunks, progress, and
            // checkpoints through the same ordered path used by other sources.
            let write_result = {
                let mut committer = SequentialWorkCommitter::new(
                    &mut writer,
                    &mut self.completed_bytes,
                    &self.group,
                    &mut self.checkpoint,
                );
                committer.commit_chunk(data.as_slice()).await
            };
            if let Err(error) = write_result {
                error!("[SFTP-CMD] Disk write error: {}", error);
                let _ = remote_file.close().await;
                self.finalize_partial_writer(&mut writer).await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::Fatal(FatalError::Config(format!(
                    "Disk write failed: {}",
                    error
                ))));
            }

            // Update download speed in RequestGroup.
            {
                let g = self.group.recover();
                // Periodic speed calculation (every ~500ms)
                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= constants::SFTP_SPEED_UPDATE_INTERVAL_MS as u128 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0);
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        // -----------------------------------------------------------------
        // Phase 7: Finalize and Cleanup
        // -----------------------------------------------------------------

        // Close remote file handle
        if let Err(e) = remote_file.close().await {
            warn!("[SFTP-CMD] Warning closing remote file: {}", e);
        }

        // Finalize disk writer (flush, sync, etc.). Completion is not valid
        // unless the local bytes have been durably finalized.
        let mut finalized_data = match writer.finalize().await {
            Ok(data) => data,
            Err(error) => {
                self.flush_checkpoint().await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::FileIo(format!(
                    "[SFTP-CMD] Failed to finalize disk writer: {error}"
                )));
            }
        };

        // aria2_original schedules ChecksumCheckIntegrityEntry after the
        // SFTP transfer, including when the local output was already complete.
        // Keep that verification at the Rust-owned completion seam so the
        // output is never marked complete before its configured checksum is
        // known to match.
        if let Some((algorithm, expected)) = checksum_config {
            let checksum = match Checksum::from_type_and_value(&algorithm, &expected) {
                Ok(checksum) => checksum,
                Err(error) => {
                    let _ = conn.disconnect().await;
                    return Err(error);
                }
            };
            let verified = if in_memory_download {
                let (data, verified) = checksum.verify_async(finalized_data).await?;
                finalized_data = data;
                verified
            } else {
                match crate::checksum::check_integrity::man::enqueue_file_checksum_for_group(
                    &crate::checksum::check_integrity::man::shared(),
                    std::sync::Arc::clone(&self.group),
                    &self.output_path,
                    self.completed_bytes,
                    checksum,
                )
                .await
                {
                    Ok(verified) => verified,
                    Err(error) => {
                        self.flush_checkpoint().await;
                        let _ = conn.disconnect().await;
                        return Err(error);
                    }
                }
            };
            if !verified {
                self.flush_checkpoint().await;
                let _ = conn.disconnect().await;
                return Err(Aria2Error::Checksum(format!(
                    "{} checksum mismatch for {}",
                    algorithm,
                    self.output_path.display()
                )));
            }
            self.group.recover().set_checksum_verified(true);
        }

        if in_memory_download {
            let group = self.group.recover();
            group.set_total_length(self.completed_bytes);
            group.set_completed_length(self.completed_bytes);
            group.set_in_memory_data(finalized_data);
        }

        // Disconnect SSH session
        if let Err(e) = conn.disconnect().await {
            warn!("[SFTP-CMD] Warning during SSH disconnect: {}", e);
        }

        self.complete_checkpoint().await;

        // Calculate final statistics
        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };

        // Mark RequestGroup as complete
        {
            let g = self.group.recover();
            g.update_progress(self.completed_bytes);
            g.update_speed(final_speed, 0);
            drop(g);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        info!(
            "[SFTP-CMD] Download complete: {} ({} bytes, {:.1} KB/s)",
            self.output_path.display(),
            self.completed_bytes,
            final_speed as f64 / 1024.0
        );

        Ok(())
    }
}

async fn connect_sftp_host(
    host: &str,
    port: u16,
    dns_cache: Option<&tokio::sync::Mutex<DnsCache>>,
    network_policy: &crate::network::OutboundNetworkPolicy,
) -> std::result::Result<TcpStream, SshError> {
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return network_policy
            .connect(std::net::SocketAddr::new(address, port))
            .await
            .map_err(|error| SshError::ConnectFailed {
                host: host.to_string(),
                port,
                message: error.to_string(),
            });
    }

    let Some(dns_cache) = dns_cache else {
        return network_policy
            .connect_host(host, port)
            .await
            .map_err(|error| SshError::ConnectFailed {
                host: host.to_string(),
                port,
                message: error.to_string(),
            });
    };

    let dns_cache = dns_cache.lock().await.clone();
    let addresses = dns_cache
        .resolve_with_refresh(host, port)
        .await
        .map_err(|error| SshError::ConnectFailed {
            host: host.to_string(),
            port,
            message: error.to_string(),
        })?;
    let mut last_error = None;
    for address in addresses {
        match network_policy.connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    Err(SshError::ConnectFailed {
        host: host.to_string(),
        port,
        message: last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "DNS lookup returned no addresses".to_string()),
    })
}

#[cfg(test)]
mod connect_tests {
    use super::*;

    #[tokio::test]
    async fn async_dns_sftp_connection_uses_the_shared_cache() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local SSH endpoint");
        let port = listener.local_addr().expect("read endpoint").port();
        let accept = tokio::spawn(async move { listener.accept().await });
        let dns_cache = tokio::sync::Mutex::new(DnsCache::new());

        let stream = connect_sftp_host(
            "localhost",
            port,
            Some(&dns_cache),
            &crate::network::OutboundNetworkPolicy::direct(),
        )
        .await
        .expect("shared DNS cache should resolve localhost and connect");

        assert_eq!(dns_cache.lock().await.len(), 1);
        drop(stream);
        accept
            .await
            .expect("accept task should finish")
            .expect("accept local connection");
    }

    #[tokio::test]
    async fn explicit_dns_server_routes_sftp_hostname_to_resolved_tcp_peer() {
        use tokio::net::UdpSocket;

        let dns_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind explicit DNS fixture");
        let dns_address = dns_socket
            .local_addr()
            .expect("read explicit DNS fixture address");
        let dns_server = tokio::spawn(async move {
            let mut request = [0u8; 512];
            for _ in 0..4 {
                let (length, peer) = dns_socket
                    .recv_from(&mut request)
                    .await
                    .expect("receive explicit DNS query");
                assert!(length >= 17, "DNS query must contain its question");
                let mut question_end = 12;
                loop {
                    let label_length = request[question_end] as usize;
                    question_end += 1;
                    if label_length == 0 {
                        question_end += 4;
                        break;
                    }
                    question_end += label_length;
                }
                let qtype =
                    u16::from_be_bytes([request[question_end - 4], request[question_end - 3]]);
                let has_address = qtype == 1;
                let mut response = Vec::with_capacity(question_end + 16);
                response.extend_from_slice(&request[..2]);
                response.extend_from_slice(&[
                    0x81,
                    0x80,
                    0,
                    1,
                    0,
                    u8::from(has_address),
                    0,
                    0,
                    0,
                    0,
                ]);
                response.extend_from_slice(&request[12..question_end]);
                if has_address {
                    response.extend_from_slice(&[
                        0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1,
                    ]);
                }
                dns_socket
                    .send_to(&response, peer)
                    .await
                    .expect("send explicit DNS response");
                if has_address {
                    break;
                }
            }
        });

        let ssh_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local SSH endpoint");
        let ssh_address = ssh_listener.local_addr().expect("read SSH endpoint");
        let accept = tokio::spawn(async move {
            ssh_listener
                .accept()
                .await
                .expect("accept SFTP TCP connection")
                .1
        });
        let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
            Duration::from_secs(2),
            vec![dns_address],
        ));

        let stream = tokio::time::timeout(
            Duration::from_secs(3),
            connect_sftp_host(
                "sftp.explicit-dns.test",
                ssh_address.port(),
                Some(&dns_cache),
                &crate::network::OutboundNetworkPolicy::direct(),
            ),
        )
        .await
        .expect("SFTP DNS-to-TCP connection should finish promptly")
        .expect("configured DNS answer should connect to the local SSH endpoint");

        assert_eq!(
            stream.peer_addr().expect("read connected peer"),
            ssh_address
        );
        assert_eq!(dns_cache.lock().await.len(), 1);
        drop(stream);
        assert_eq!(
            accept.await.expect("SSH accept task should finish").ip(),
            ssh_address.ip()
        );
        tokio::time::timeout(Duration::from_secs(2), dns_server)
            .await
            .expect("DNS fixture should finish promptly")
            .expect("DNS fixture task should not fail");
    }
}

async fn connect_with_timeout<T, F>(
    host: &str,
    port: u16,
    timeout: Duration,
    connection: F,
) -> std::result::Result<T, SshError>
where
    F: Future<Output = std::result::Result<T, SshError>>,
{
    tokio::time::timeout(timeout, connection)
        .await
        .unwrap_or_else(|_| {
            Err(SshError::ConnectTimeout {
                host: host.to_string(),
                port,
                timeout_secs: timeout.as_secs(),
            })
        })
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};

    #[tokio::test]
    async fn connect_timeout_covers_a_pending_connection_and_keeps_target_context() {
        let result = connect_with_timeout(
            "sftp.example",
            2222,
            Duration::from_millis(1),
            std::future::pending::<std::result::Result<(), SshError>>(),
        )
        .await;

        assert!(matches!(
            result,
            Err(SshError::ConnectTimeout {
                host,
                port: 2222,
                timeout_secs: 0,
            }) if host == "sftp.example"
        ));
    }

    #[tokio::test]
    async fn lifecycle_removal_interrupts_pending_sftp_setup() {
        let command = SftpDownloadCommand::new(
            GroupId::new(1),
            "sftp://user:password@example.com/file.bin",
            &DownloadOptions::default(),
            None,
            None,
        )
        .expect("construct SFTP command");
        let group = Arc::clone(&command.group);
        let removal = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            group
                .write()
                .unwrap()
                .remove()
                .expect("remove pending SFTP task");
        });

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            command.await_with_lifecycle(std::future::pending::<Result<()>>()),
        )
        .await
        .expect("lifecycle removal should interrupt setup promptly");

        assert!(matches!(result, Err(Aria2Error::DownloadFailed(_))));
        removal.await.expect("removal task should finish");
    }
}
