//! FTP download execution logic.
//!
//! Implements the `Command` trait for `FtpDownloadCommand` and the
//! single-attempt download procedure (connect, authenticate, transfer,
//! finalize).

use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

use crate::checksum::checksum::{Checksum, verify_file};
use crate::constants;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DiskWriter, new_sequential_download_writer};
use crate::network::ConnectionContext;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use super::control::RawFtpControl;
use super::types::FtpDownloadCommand;

pub(super) struct FtpAttemptError {
    pub(super) source: Aria2Error,
    pub(super) failed_control: Option<ConnectionContext>,
}

impl FtpAttemptError {
    pub(super) fn control(source: Aria2Error, context: ConnectionContext) -> Self {
        Self {
            source,
            failed_control: Some(context),
        }
    }
}

impl From<Aria2Error> for FtpAttemptError {
    fn from(source: Aria2Error) -> Self {
        Self {
            source,
            failed_control: None,
        }
    }
}

#[async_trait]
impl Command for FtpDownloadCommand {
    /// Execute the FTP download with full lifecycle management
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        info!(
            "FTP download starting: {}:{} -> {}",
            self.host,
            self.port,
            self.output_path.display()
        );

        let in_memory_download = self.group.recover().is_in_memory_download();

        // Create output directory if needed
        if !in_memory_download
            && let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        // Retry loop for transient errors. The policy counts total attempts,
        // matching aria2's `--max-tries` contract.
        let mut attempts = 0u32;
        loop {
            let attempt_index = attempts;
            match self.execute_single_attempt(attempt_index).await {
                Ok(_) => {
                    info!(
                        "FTP download completed successfully: {} ({} bytes)",
                        self.output_path.display(),
                        self.completed_bytes
                    );
                    return Ok(());
                }
                Err(attempt_error) => {
                    self.flush_checkpoint().await;
                    let FtpAttemptError {
                        source: mut e,
                        failed_control,
                    } = attempt_error;
                    if matches!(
                        e,
                        Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
                    ) {
                        e = self.group.recover().file_not_found_error();
                    }
                    let reject_control = matches!(
                        e,
                        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
                            | Aria2Error::Recoverable(RecoverableError::Timeout)
                    );
                    if reject_control && let Some(context) = failed_control.as_ref() {
                        if let Some(cache) = self.dns_cache.as_ref() {
                            cache.lock().await.mark_bad_context(context);
                        }
                        self.resolved_addresses
                            .retain(|address| *address != context.peer_addr);
                        tracing::debug!(
                            host = %context.endpoint.hostname(),
                            peer = %context.peer_addr,
                            "FTP control connection failed; peer was rejected"
                        );
                    }
                    // Check if this is a retry-worthy error
                    let should_retry = match &e {
                        Aria2Error::Recoverable(RecoverableError::ResourceNotFound) => {
                            self.retry_policy
                                .can_retry_after(attempts.saturating_add(1))
                                && self.group.recover().can_retry_file_not_found()
                        }
                        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            ..
                        })
                        | Aria2Error::Recoverable(RecoverableError::Timeout) => self
                            .retry_policy
                            .can_retry_after(attempts.saturating_add(1)),
                        _ => false,
                    };

                    if should_retry {
                        attempts = attempts.saturating_add(1);
                        let wait = self.retry_policy.compute_wait(attempts).unwrap_or_default();
                        warn!(
                            "FTP download failed (attempt {}/{}), retrying in {:?}: {}",
                            attempts,
                            self.retry_policy.max_tries(),
                            wait,
                            e
                        );
                        self.wait_for_retry(wait).await?;

                        // Reset state for retry
                        self.completed_bytes = 0;
                        continue;
                    }

                    // Non-retryable error or max retries exceeded
                    error!(
                        "FTP download failed permanently after {} attempts: {}",
                        attempts.saturating_add(1),
                        e
                    );
                    return Err(e);
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        self.flush_checkpoint().await;
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 || self.started {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
    }

    fn request_group(
        &self,
    ) -> Option<std::sync::Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>>
    {
        Some(std::sync::Arc::clone(&self.group))
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(
            constants::FTP_DEFAULT_COMMAND_TIMEOUT_SECS,
        ))
    }
}

impl FtpDownloadCommand {
    /// Wait between retry attempts while still honoring RequestGroup controls.
    /// A plain sleep would delay pause/remove handling for the full configured
    /// retry interval, which can be several minutes.
    pub(super) async fn wait_for_retry(&self, wait: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let halt_message = {
                let group = self.group.recover();
                if group.is_removed() {
                    Some("Download cancelled by user")
                } else if group.is_paused_flag() {
                    Some("Download paused")
                } else if group.is_force_halt_requested() || group.is_halt_requested() {
                    Some("FTP download halted")
                } else {
                    None
                }
            };
            if let Some(message) = halt_message {
                return Err(Aria2Error::DownloadFailed(message.into()));
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(());
            }
            tokio::time::sleep((deadline - now).min(Duration::from_millis(50))).await;
        }
    }

    /// Apply the optional remote timestamp after the output handle has been
    /// finalized. This mirrors the original post-download file-attribute
    /// update while keeping failures non-fatal, as aria2 does.
    pub(super) fn apply_remote_time(
        &self,
        remote_modified_time: Option<SystemTime>,
        in_memory_download: bool,
    ) {
        if in_memory_download {
            return;
        }

        let Some(remote_modified_time) = remote_modified_time else {
            return;
        };

        let result = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.output_path)
            .and_then(|file| file.set_modified(remote_modified_time));
        if let Err(error) = result {
            warn!(
                path = %self.output_path.display(),
                %error,
                "Failed to apply FTP remote modification time"
            );
        }
    }

    pub(super) async fn flush_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.as_mut() {
            let _ = self.group.recover().take_save_control_file_request();
            checkpoint.update(self.completed_bytes, true).await;
        }
    }

    pub(super) async fn complete_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.complete().await;
        }
    }

    pub(super) async fn finalize_partial_writer(&mut self, writer: &mut Box<dyn DiskWriter>) {
        let _ = writer.finalize().await;
        self.flush_checkpoint().await;
    }

    /// Execute a single download attempt
    async fn execute_single_attempt(
        &mut self,
        attempt_index: u32,
    ) -> std::result::Result<(), FtpAttemptError> {
        let in_memory_download = self.group.recover().is_in_memory_download();
        let proxy_config = self.ftp_proxy_config().map_err(FtpAttemptError::from)?;
        if let Some((proxy, crate::ftp::connection::ProxyMethod::Get)) = proxy_config.as_ref() {
            return self.execute_proxy_get_attempt(proxy).await;
        }

        let control_address = if proxy_config.is_some() {
            // The proxy resolves the FTP origin in tunnel mode. Resolving it
            // locally would reject valid proxy-only DNS names and would make
            // proxy failures look like origin failures.
            std::net::SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            if self.resolved_addresses.is_empty() {
                self.refresh_control_addresses().await?;
            }
            self.resolved_addresses[attempt_index as usize % self.resolved_addresses.len()]
        };
        let context = ConnectionContext::new(&self.host, self.port, control_address);
        let host = self.host.clone();
        let port = self.port;
        let ftps_config = self.ftps_config.clone();
        let ftps_implicit = self.ftps_implicit;
        let connect_timeout = self.connect_timeout;
        let proxy_for_connection = proxy_config.as_ref().map(|(proxy, _)| proxy.clone());
        let connect_result = tokio::time::timeout(connect_timeout, async move {
            if let Some(proxy) = proxy_for_connection.as_ref() {
                RawFtpControl::connect_via_http_proxy(
                    &host,
                    port,
                    proxy,
                    ftps_config.as_ref(),
                    ftps_implicit,
                )
                .await
            } else if let Some(config) = ftps_config.as_ref() {
                if ftps_implicit {
                    RawFtpControl::connect_ftps_implicit_at(&host, port, control_address, config)
                        .await
                } else {
                    RawFtpControl::connect_ftps_explicit_at(&host, port, control_address, config)
                        .await
                }
            } else {
                RawFtpControl::connect_at(&host, port, control_address).await
            }
        })
        .await;
        let mut ctrl = match connect_result {
            Ok(Ok(ctrl)) => ctrl,
            Ok(Err(error)) if proxy_config.is_some() => {
                return Err(FtpAttemptError::from(error));
            }
            Ok(Err(error)) => return Err(FtpAttemptError::control(error, context)),
            Err(_) => {
                if proxy_config.is_some() {
                    return Err(FtpAttemptError::from(Aria2Error::Recoverable(
                        RecoverableError::Timeout,
                    )));
                }
                return Err(FtpAttemptError::control(
                    Aria2Error::Recoverable(RecoverableError::Timeout),
                    context,
                ));
            }
        };
        self.last_connection_context = Some(ctrl.connection_context().clone());
        self.group
            .recover()
            .set_connection_context(ctrl.connection_context().clone());

        // Step 2: Authenticate
        ctrl.authenticate(&self.username, &self.password).await?;

        // Step 3: Set binary transfer mode
        ctrl.set_binary_mode().await?;

        // Step 4: Resolve the URI directory and retain only the file name for
        // SIZE/RETR, matching the original FTP command sequence.
        let file_path = ctrl.prepare_remote_path(&self.remote_path).await?;

        // The original queries MDTM after CWD traversal and before SIZE when
        // remote-time is enabled. A missing/unsupported MDTM response does
        // not make an otherwise valid FTP download fail.
        let remote_modified_time = if self.group.recover().options().remote_time {
            ctrl.get_modification_time(&file_path).await?
        } else {
            None
        };

        // Step 5: Probe file size
        let file_size = ctrl.get_file_size(&file_path).await?;

        // aria2_original's dry-run path stops after metadata discovery. It
        // marks the file as found without opening a data connection or
        // issuing REST/RETR, so no local output is created.
        if self.group.recover().options().dry_run {
            let discovered_length = file_size.unwrap_or_default();
            self.completed_bytes = discovered_length;
            {
                let g = self.group.recover();
                g.set_total_length(discovered_length);
                g.update_progress(discovered_length);
                g.set_checksum_verified(true);
            }
            self.group.recover_mut().complete()?;
            ctrl.quit().await.ok();
            return Ok(());
        }

        // Reconcile the local file with SIZE before allocation/REST/RETR.
        // This mirrors FtpNegotiationCommand::onFileSizeDetermined(): a
        // complete local file is terminal, while an oversized file must not
        // be used as a resume prefix.
        let local_size = if in_memory_download {
            0
        } else {
            std::fs::metadata(&self.output_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        };
        let continue_download = self.group.recover().options().continue_download;
        let mut restart_from_zero = false;
        if let Some(actual_size) = file_size {
            let resume_input_length =
                crate::engine::progress_checkpoint::ProgressCheckpoint::resume_input_length(
                    &self.output_path,
                    local_size,
                    continue_download,
                    actual_size,
                )
                .await;
            {
                let g = self.group.recover();
                g.validate_total_length(g.total_length(), actual_size)
                    .map_err(FtpAttemptError::from)?;
                g.set_total_length(actual_size);
            }

            if in_memory_download {
                self.checkpoint = None;
            } else {
                self.checkpoint = Some(
                    crate::engine::progress_checkpoint::ProgressCheckpoint::open(
                        &self.output_path,
                        actual_size,
                        resume_input_length,
                    )
                    .await,
                );
                self.resume_offset = self
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.resume_offset(resume_input_length))
                    .unwrap_or(resume_input_length);
            }

            if !in_memory_download && local_size == actual_size && self.resume_offset == actual_size
            {
                let checksum_valid = {
                    let checksum_config = self.group.recover().options().checksum.clone();
                    match checksum_config {
                        Some((algorithm, expected)) => {
                            let hash_type =
                                crate::checksum::message_digest::HashType::from_str(&algorithm)
                                    .ok_or_else(|| {
                                        Aria2Error::Parse(format!(
                                            "unknown checksum algorithm: {}",
                                            algorithm
                                        ))
                                    })?;
                            let checksum = Checksum::new(hash_type, &expected)?;
                            verify_file(&self.output_path, &checksum).await?
                        }
                        None => true,
                    }
                };

                if checksum_valid {
                    self.resume_offset = actual_size;
                    self.completed_bytes = actual_size;
                    {
                        let g = self.group.recover();
                        g.update_progress(actual_size);
                    }
                    if self.group.recover().options().checksum.is_some() {
                        self.group.recover().set_checksum_verified(true);
                    }
                    self.apply_remote_time(remote_modified_time, in_memory_download);
                    self.group.recover_mut().complete()?;
                    self.complete_checkpoint().await;
                    info!(
                        path = %self.output_path.display(),
                        size = actual_size,
                        "FTP target already matches remote SIZE and checksum"
                    );
                    ctrl.quit().await.ok();
                    return Ok(());
                }

                warn!(
                    path = %self.output_path.display(),
                    "FTP target checksum mismatch; restarting from byte zero"
                );
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&self.output_path)
                    .and_then(|file| file.set_len(0))
                    .map_err(|error| {
                        FtpAttemptError::from(Aria2Error::FileIo(format!(
                            "truncate checksum-mismatched FTP target {}: {}",
                            self.output_path.display(),
                            error
                        )))
                    })?;
                self.resume_offset = 0;
                self.flush_checkpoint().await;
                restart_from_zero = true;
            }

            if in_memory_download {
                self.resume_offset = 0;
            } else if local_size > actual_size {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&self.output_path)
                    .and_then(|file| file.set_len(0))
                    .map_err(|error| {
                        FtpAttemptError::from(Aria2Error::FileIo(format!(
                            "truncate oversized FTP target {}: {}",
                            self.output_path.display(),
                            error
                        )))
                    })?;
                self.resume_offset = 0;
                self.flush_checkpoint().await;
            } else if !restart_from_zero {
                self.resume_offset = self
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.resume_offset(resume_input_length))
                    .unwrap_or(resume_input_length);
            }
        } else if !in_memory_download {
            self.resume_offset = if continue_download { local_size } else { 0 };
        }

        // Step 6: Allocate the destination before RETR, matching the C++
        // FileAllocationEntry command chain used by FTP downloads.
        let allocation =
            crate::filesystem::file_allocation::AllocationStrategy::from_str(&self.file_allocation);
        if !in_memory_download
            && allocation != crate::filesystem::file_allocation::AllocationStrategy::None
            && file_size.unwrap_or(0) > 0
        {
            let gid = { self.group.recover().gid().value() };
            crate::filesystem::file_allocation_man::enqueue_path(
                &crate::filesystem::file_allocation_man::shared(),
                &self.output_path,
                file_size.unwrap_or(0),
                allocation,
                self.secure_falloc,
                gid,
            )
            .await
            .map_err(FtpAttemptError::from)?;
        }

        // Step 7: Negotiate the data connection mode before REST/RETR.
        let passive_stream = if self.passive_mode {
            Some(ctrl.enter_passive_mode().await?)
        } else {
            None
        };
        let active_listener = if self.passive_mode {
            None
        } else {
            Some(ctrl.enter_active_mode().await?)
        };

        // Step 8: Set the resume offset after data-channel preparation. The
        // original sends REST 0 as well; only a non-zero rejection restarts
        // the local partial file.
        let resume_accepted = ctrl.set_resume_offset(self.resume_offset).await?;
        let write_offset = if resume_accepted {
            self.resume_offset
        } else {
            0
        };
        if !resume_accepted {
            // REST rejection means RETR will send the complete object. Make
            // the restart explicit so stale bytes cannot survive past EOF.
            self.resume_offset = 0;
            std::fs::OpenOptions::new()
                .write(true)
                .open(&self.output_path)
                .and_then(|file| file.set_len(0))
                .map_err(|error| {
                    FtpAttemptError::from(Aria2Error::FileIo(format!(
                        "truncate FTP target after REST rejection {}: {}",
                        self.output_path.display(),
                        error
                    )))
                })?;
        }

        // Step 9: Initiate file transfer (RETR command).
        ctrl.initiate_retr(&file_path).await?;

        // Step 10: Establish the data connection. In active mode the server
        // connects back after RETR; never attempt a client-side connect.
        let data_stream = if let Some(stream) = passive_stream {
            stream
        } else {
            let listener =
                active_listener.expect("active listener is present when passive mode is disabled");
            tokio::time::timeout(
                Duration::from_secs(constants::FTP_DATA_CONNECTION_TIMEOUT_SECS),
                listener.accept(),
            )
            .await
            .map_err(|_| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: "Active FTP data connection timeout".into(),
                })
            })?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Active FTP data connection failed: {}", e),
                })
            })?
            .0
        };

        // Upgrade the data channel after the server accepted RETR. For plain
        // FTP this preserves the TCP stream; FTPS performs the PROT P TLS
        // handshake before any payload bytes are read.
        let mut data_stream = ctrl.secure_data_stream(data_stream).await?;
        let _ = data_stream.set_nodelay(true); // Ignore error if not supported

        // Step 11: Select a disk or memory writer, then apply optional rate
        // limiting. The memory writer is the FTP/SFTP equivalent of aria2's
        // MemoryPreDownloadHandler and never opens output_path.
        let raw_writer = new_sequential_download_writer(
            &self.output_path,
            in_memory_download,
            write_offset,
            file_size,
        );
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
                .map(|rate| {
                    debug!("Rate limiting enabled: {} bytes/sec", rate);
                    RateLimiter::new(&RateLimiterConfig::new(Some(rate), None))
                })
                .unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = self.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            raw_writer
        };

        // Seek to resume offset if resuming
        // Note: DiskWriter trait doesn't support seek, so for resume we rely on
        // the FTP REST command to tell server to start from the offset,
        // and data will be appended to existing file if it exists
        if write_offset > 0 {
            debug!(
                "Resume offset: {} bytes (using FTP REST command)",
                write_offset
            );
        }

        // Step 12: Data receive loop with progress tracking. Existing bytes
        // are part of the logical completed length when resuming.
        self.completed_bytes = write_offset;
        {
            let g = self.group.recover();
            g.update_progress(write_offset);
        }
        let mut buffer = vec![0u8; constants::FTP_BUFFER_SIZE];
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        info!("Starting data reception from FTP server");

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
                        "FTP download halted"
                    }
                };
                drop(data_stream);
                let _ = writer.finalize().await;
                self.flush_checkpoint().await;
                ctrl.abort_transfer().await;
                ctrl.quit().await.ok();
                return Err(FtpAttemptError::from(Aria2Error::DownloadFailed(
                    halt_error.into(),
                )));
            }

            let bytes_read = match data_stream.read(&mut buffer).await {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    use std::io::ErrorKind;
                    let error = match error.kind() {
                        ErrorKind::Interrupted
                        | ErrorKind::WouldBlock
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                        | ErrorKind::TimedOut => {
                            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                                message: format!("Data read error (transient): {}", error),
                            })
                        }
                        _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: format!("Data read error: {}", error),
                        }),
                    };
                    drop(data_stream);
                    self.finalize_partial_writer(&mut writer).await;
                    ctrl.abort_transfer().await;
                    ctrl.quit().await.ok();
                    return Err(FtpAttemptError::from(error));
                }
            };

            if bytes_read == 0 {
                debug!("End of data stream reached");
                break;
            }

            // Write to disk (with rate limiting if enabled)
            if let Err(error) = writer.write(&buffer[..bytes_read]).await {
                drop(data_stream);
                self.finalize_partial_writer(&mut writer).await;
                ctrl.abort_transfer().await;
                ctrl.quit().await.ok();
                return Err(FtpAttemptError::from(error));
            }
            self.completed_bytes += bytes_read as u64;
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                let save_requested = self.group.recover().take_save_control_file_request();
                checkpoint
                    .update(self.completed_bytes, save_requested)
                    .await;
            }

            // Update progress in request group
            {
                let g = self.group.recover();
                g.update_progress(self.completed_bytes);

                // Update speed calculation every 500ms
                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= constants::FTP_SPEED_UPDATE_INTERVAL_MS as u128 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = if elapsed.as_secs_f64() > 0.0 {
                        (delta as f64 / elapsed.as_secs_f64()) as u64
                    } else {
                        0
                    };
                    g.update_speed(speed, 0);
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        if let Some(expected_size) = file_size
            && self.completed_bytes != expected_size
        {
            drop(data_stream);
            self.finalize_partial_writer(&mut writer).await;
            ctrl.abort_transfer().await;
            ctrl.quit().await.ok();
            return Err(FtpAttemptError::from(Aria2Error::FtpProtocol(format!(
                "FTP transfer length mismatch: expected {}, got {}",
                expected_size, self.completed_bytes
            ))));
        }

        // Step 13: Cleanup and finalize
        drop(data_stream); // Close data connection

        // Finalize disk writer (flush buffers, etc.)
        let finalized_data = match writer.finalize().await {
            Ok(data) => data,
            Err(error) => {
                self.flush_checkpoint().await;
                ctrl.abort_transfer().await;
                ctrl.quit().await.ok();
                return Err(FtpAttemptError::from(Aria2Error::Fatal(
                    FatalError::Config(format!("Finalize writer failed: {}", error)),
                )));
            }
        };
        drop(writer);

        let checksum_config = self.group.recover().options().checksum.clone();
        if let Some((algorithm, expected)) = checksum_config {
            let hash_type = crate::checksum::message_digest::HashType::from_str(&algorithm)
                .ok_or_else(|| {
                    Aria2Error::Parse(format!("unknown checksum algorithm: {}", algorithm))
                })?;
            let checksum = Checksum::new(hash_type, &expected)?;
            let verified = if in_memory_download {
                checksum.verify(&finalized_data)
            } else {
                verify_file(&self.output_path, &checksum).await?
            };
            if !verified {
                return Err(FtpAttemptError::from(Aria2Error::Checksum(format!(
                    "{} checksum mismatch for {}",
                    algorithm,
                    self.output_path.display()
                ))));
            }
            self.group.recover().set_checksum_verified(true);
        }

        self.apply_remote_time(remote_modified_time, in_memory_download);

        if in_memory_download {
            let group = self.group.recover();
            group.set_total_length(self.completed_bytes);
            group.set_completed_length(self.completed_bytes);
            group.set_in_memory_data(finalized_data);
        }

        // Read transfer completion response from control channel
        ctrl.read_transfer_complete().await?;

        self.complete_checkpoint().await;

        // Disconnect gracefully
        ctrl.quit().await.ok();

        // Calculate final statistics
        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };

        // Update final status in request group
        {
            let g = self.group.recover();
            g.update_progress(self.completed_bytes);
            g.update_speed(final_speed, 0);
            drop(g);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        Ok(())
    }

    async fn refresh_control_addresses(&mut self) -> Result<()> {
        if let Some(cache) = self.dns_cache.as_ref() {
            let mut cache = cache.lock().await;
            let addresses = cache.resolve_with_refresh(&self.host, self.port).await?;
            if addresses.is_empty() {
                return Err(Aria2Error::NameResolve(format!(
                    "No usable address for {}:{}",
                    self.host, self.port
                )));
            }
            self.resolved_addresses = addresses;
            return Ok(());
        }

        self.resolved_addresses = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|error| {
                Aria2Error::NameResolve(format!(
                    "DNS resolution failed for {}:{}: {}",
                    self.host, self.port, error
                ))
            })?
            .collect();
        if self.resolved_addresses.is_empty() {
            return Err(Aria2Error::NameResolve(format!(
                "No address resolved for {}:{}",
                self.host, self.port
            )));
        }
        Ok(())
    }
}
