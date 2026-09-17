//! FTP control-plane attempt preparation.

use std::sync::Arc;
use std::time::Duration;

use crate::checksum::checksum::Checksum;
use crate::constants;
use tracing::{info, warn};

use crate::error::{Aria2Error, RecoverableError};
use crate::network::ConnectionContext;
use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use super::control::RawFtpControl;
use super::execution::FtpAttemptError;
use super::types::FtpDownloadCommand;

impl FtpDownloadCommand {
    pub(super) async fn execute_single_attempt(
        &mut self,
        attempt_index: u32,
    ) -> std::result::Result<(), FtpAttemptError> {
        let connection_guard = ActiveConnectionGuard::new(Arc::clone(&self.group));
        connection_guard.set(1);
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

        // Step 3: Set the configured transfer representation.
        let ftp_type = self.group.recover().options().ftp_type.clone();
        ctrl.set_transfer_type(&ftp_type).await?;

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
                            crate::checksum::check_integrity::man::enqueue_file_checksum_for_group(
                                &crate::checksum::check_integrity::man::shared(),
                                std::sync::Arc::clone(&self.group),
                                &self.output_path,
                                actual_size,
                                checksum,
                            )
                            .await?
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
        let data_stream = ctrl.secure_data_stream(data_stream).await?;
        let _ = data_stream.set_nodelay(true); // Ignore error if not supported

        self.receive_data_transfer(
            ctrl,
            data_stream,
            file_size,
            in_memory_download,
            remote_modified_time,
            write_offset,
        )
        .await
    }
}
