//! FTP download execution logic.
//!
//! Implements the `Command` trait for `FtpDownloadCommand` and the
//! single-attempt download procedure (connect, authenticate, transfer,
//! finalize).

use std::time::{Duration, Instant};

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

struct FtpAttemptError {
    source: Aria2Error,
    failed_control: Option<ConnectionContext>,
}

impl FtpAttemptError {
    fn control(source: Aria2Error, context: ConnectionContext) -> Self {
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
                    let FtpAttemptError {
                        source: e,
                        failed_control,
                    } = attempt_error;
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
                    let is_retryable_error = matches!(
                        &e,
                        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
                            | Aria2Error::Recoverable(RecoverableError::Timeout)
                    );
                    let should_retry = is_retryable_error
                        && self
                            .retry_policy
                            .can_retry_after(attempts.saturating_add(1));

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
                        tokio::time::sleep(wait).await;

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
    /// Execute a single download attempt
    async fn execute_single_attempt(
        &mut self,
        attempt_index: u32,
    ) -> std::result::Result<(), FtpAttemptError> {
        let in_memory_download = self.group.recover().is_in_memory_download();

        if self.resolved_addresses.is_empty() {
            self.refresh_control_addresses().await?;
        }
        let control_address =
            self.resolved_addresses[attempt_index as usize % self.resolved_addresses.len()];
        let context = ConnectionContext::new(&self.host, self.port, control_address);
        let host = self.host.clone();
        let port = self.port;
        let ftps_config = self.ftps_config.clone();
        let ftps_implicit = self.ftps_implicit;
        let connect_result = tokio::time::timeout(
            Duration::from_secs(constants::FTP_DEFAULT_COMMAND_TIMEOUT_SECS),
            async move {
                if let Some(config) = ftps_config.as_ref() {
                    if ftps_implicit {
                        RawFtpControl::connect_ftps_implicit_at(
                            &host,
                            port,
                            control_address,
                            config,
                        )
                        .await
                    } else {
                        RawFtpControl::connect_ftps_explicit_at(
                            &host,
                            port,
                            control_address,
                            config,
                        )
                        .await
                    }
                } else {
                    RawFtpControl::connect_at(&host, port, control_address).await
                }
            },
        )
        .await;
        let mut ctrl = match connect_result {
            Ok(Ok(ctrl)) => ctrl,
            Ok(Err(error)) => return Err(FtpAttemptError::control(error, context)),
            Err(_) => {
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

        // Step 4: Probe file size
        let file_size = ctrl.get_file_size(&self.remote_path).await?;

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
        let mut restart_from_zero = false;
        if let Some(actual_size) = file_size {
            {
                let g = self.group.recover();
                g.validate_total_length(g.total_length(), actual_size)
                    .map_err(FtpAttemptError::from)?;
                g.set_total_length(actual_size);
            }

            if !in_memory_download && local_size == actual_size {
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
                    self.group.recover_mut().complete()?;
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
            } else if !restart_from_zero {
                self.resume_offset = local_size;
            }
        }

        // Step 5: Allocate the destination before RETR, matching the C++
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

        // Step 6: Set resume offset if applicable. If the server rejects REST,
        // restart from zero and truncate the stale local partial file.
        let resume_accepted = if in_memory_download {
            true
        } else if self.resume_offset > 0 {
            ctrl.set_resume_offset(self.resume_offset).await?
        } else {
            true
        };
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

        // Step 7: Negotiate the data connection mode before RETR.
        let passive_port = if self.passive_mode {
            Some(ctrl.enter_passive_mode().await?)
        } else {
            None
        };
        let active_listener = if self.passive_mode {
            None
        } else {
            Some(ctrl.enter_active_mode().await?)
        };

        // Step 7: Initiate file transfer (RETR command).
        ctrl.initiate_retr(&self.remote_path).await?;

        // Step 8: Establish the data connection. In active mode the server
        // connects back after RETR; never attempt a client-side connect.
        let data_stream = if let Some(data_port) = passive_port {
            let data_addr =
                std::net::SocketAddr::new(ctrl.connection_context().peer_addr.ip(), data_port);
            tokio::time::timeout(
                Duration::from_secs(constants::FTP_DATA_CONNECTION_TIMEOUT_SECS),
                tokio::net::TcpStream::connect(data_addr),
            )
            .await
            .map_err(|_| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Data connection timeout via {}",
                        ctrl.connection_context().peer_addr
                    ),
                })
            })?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Data connection failed via {}: {}",
                        ctrl.connection_context().peer_addr,
                        e
                    ),
                })
            })?
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

        // Step 9: Select a disk or memory writer, then apply optional rate
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

        // Step 10: Data receive loop with progress tracking. Existing bytes
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
            let bytes_read = data_stream.read(&mut buffer).await.map_err(|e| {
                // Classify IO errors
                use std::io::ErrorKind;
                match e.kind() {
                    ErrorKind::Interrupted
                    | ErrorKind::WouldBlock
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut => {
                        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: format!("Data read error (transient): {}", e),
                        })
                    }
                    _ => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("Data read error: {}", e),
                    }),
                }
            })?;

            if bytes_read == 0 {
                debug!("End of data stream reached");
                break;
            }

            // Write to disk (with rate limiting if enabled)
            writer.write(&buffer[..bytes_read]).await?;
            self.completed_bytes += bytes_read as u64;

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
            return Err(FtpAttemptError::from(Aria2Error::FtpProtocol(format!(
                "FTP transfer length mismatch: expected {}, got {}",
                expected_size, self.completed_bytes
            ))));
        }

        // Step 11: Cleanup and finalize
        drop(data_stream); // Close data connection

        // Finalize disk writer (flush buffers, etc.)
        let finalized_data = writer.finalize().await.map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!("Finalize writer failed: {}", e)))
        })?;

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

        if in_memory_download {
            let group = self.group.recover();
            group.set_total_length(self.completed_bytes);
            group.set_completed_length(self.completed_bytes);
            group.set_in_memory_data(finalized_data);
        }

        // Read transfer completion response from control channel
        ctrl.read_transfer_complete().await?;

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
            let mut addresses = cache.resolve(&self.host, self.port).await?;
            if addresses.is_empty() {
                cache.remove_cached(&self.host, self.port);
                addresses = cache.resolve(&self.host, self.port).await?;
            }
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
