//! FTP download execution logic (retry loop and single-attempt data transfer).

use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;

use super::control::RawFtpControl;
use super::FtpDownloadCommand;

// ---------------------------------------------------------------------------
// Public entry points (called from thin wrappers in mod.rs)
// ---------------------------------------------------------------------------

/// Execute the FTP download with full lifecycle management including retries.
pub(super) async fn execute(cmd: &mut FtpDownloadCommand) -> Result<()> {
    if !cmd.started {
        cmd.group.recover_mut().start()?;
        cmd.started = true;
    }

    info!(
        "FTP download starting: {}:{} -> {}",
        cmd.host,
        cmd.port,
        cmd.output_path.display()
    );

    // Create output directory if needed
    if let Some(parent) = cmd.output_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "mkdir failed: {}",
                    e
                )))
            })?;
        }
    }

    // Retry loop for transient errors
    loop {
        match execute_single_attempt(cmd).await {
            Ok(()) => {
                info!(
                    "FTP download completed successfully: {} ({} bytes)",
                    cmd.output_path.display(),
                    cmd.completed_bytes
                );
                return Ok(());
            }
            Err(e) => {
                // Check if this is a retry-worthy error
                let should_retry = matches!(
                    e,
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
                        | Aria2Error::Recoverable(RecoverableError::Timeout)
                ) && cmd.current_retry < cmd.max_retries;

                if should_retry {
                    cmd.current_retry += 1;
                    let wait_ms =
                        constants::FTP_BASE_RETRY_WAIT_MS * (1 << (cmd.current_retry - 1));
                    warn!(
                        "FTP download failed (attempt {}/{}), retrying in {}ms: {}",
                        cmd.current_retry, cmd.max_retries, wait_ms, e
                    );
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;

                    // Reset state for retry
                    cmd.completed_bytes = 0;
                    continue;
                }

                // Non-retryable error or max retries exceeded
                error!(
                    "FTP download failed permanently after {} attempts: {}",
                    cmd.current_retry + 1,
                    e
                );
                return Err(e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Single attempt
// ---------------------------------------------------------------------------

/// Execute a single download attempt.
async fn execute_single_attempt(cmd: &mut FtpDownloadCommand) -> Result<()> {
    // Step 1: Connect to FTP server
    let mut ctrl = RawFtpControl::connect(&cmd.host, cmd.port).await?;

    // Step 2: Authenticate
    ctrl.authenticate(&cmd.username, &cmd.password).await?;

    // Step 3: Set binary transfer mode
    ctrl.set_binary_mode().await?;

    // Step 4: Probe file size
    let file_size = ctrl.get_file_size(&cmd.remote_path).await?;

    // Update total length in request group
    {
        let g = cmd.group.recover();
        g.set_total_length(file_size.unwrap_or(0));
    }

    // Step 5: Set resume offset if applicable
    if cmd.resume_offset > 0 {
        ctrl.set_resume_offset(cmd.resume_offset).await?;
    }

    // Step 6: Negotiate data connection mode
    let (data_host, data_port) = if cmd.passive_mode {
        ctrl.enter_passive_mode().await?
    } else {
        // Active mode would go here (not fully implemented in this version)
        return Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: "Active mode not yet implemented".into(),
            },
        ));
    };

    // Step 7: Initiate file transfer (RETR)
    ctrl.initiate_retr(&cmd.remote_path).await?;

    // Step 8: Connect to data port
    let data_addr: std::net::SocketAddr = format!("{}:{}", data_host, data_port)
        .parse()
        .map_err(|_| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "Invalid data address".into(),
            })
        })?;

    let mut data_stream = tokio::time::timeout(
        Duration::from_secs(constants::FTP_DATA_CONNECTION_TIMEOUT_SECS),
        tokio::net::TcpStream::connect(data_addr),
    )
    .await
    .map_err(|_| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "Data connection timeout".into(),
        })
    })?
    .map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Data connection failed: {}", e),
        })
    })?;

    // Set TCP no-delay on data connection
    let _ = data_stream.set_nodelay(true); // Ignore error if not supported

    // Step 9: Setup disk writer with optional rate limiting
    let raw_writer = DefaultDiskWriter::new(&cmd.output_path);
    let rate_limit = {
        let g = cmd.group.recover();
        g.options().max_download_limit
    };
    let mut writer: Box<dyn DiskWriter> = if let Some(rate) = rate_limit.filter(|&r| r > 0) {
        debug!("Rate limiting enabled: {} bytes/sec", rate);
        Box::new(ThrottledWriter::new(
            raw_writer,
            RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)),
        ))
    } else {
        Box::new(raw_writer)
    };

    // Seek to resume offset if resuming
    // Note: DiskWriter trait doesn't support seek, so for resume we rely on
    // the FTP REST command to tell server to start from the offset,
    // and data will be appended to existing file if it exists
    if cmd.resume_offset > 0 {
        debug!(
            "Resume offset: {} bytes (using FTP REST command)",
            cmd.resume_offset
        );
    }

    // Step 10: Data receive loop with progress tracking
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
        cmd.completed_bytes += bytes_read as u64;

        // Update progress in request group
        {
            let g = cmd.group.recover();
            g.update_progress(cmd.completed_bytes);

            // Update speed calculation every 500ms
            let elapsed = last_speed_update.elapsed();
            if elapsed.as_millis() >= constants::FTP_SPEED_UPDATE_INTERVAL_MS as u128 {
                let delta = cmd.completed_bytes - last_completed;
                let speed = if elapsed.as_secs_f64() > 0.0 {
                    (delta as f64 / elapsed.as_secs_f64()) as u64
                } else {
                    0
                };
                g.update_speed(speed, 0);
                last_speed_update = Instant::now();
                last_completed = cmd.completed_bytes;
            }
        }
    }

    // Step 11: Cleanup and finalize
    drop(data_stream); // Close data connection

    // Finalize disk writer (flush buffers, etc.)
    writer.finalize().await.map_err(|e| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Finalize writer failed: {}",
            e
        )))
    })?;

    // Read transfer completion response from control channel
    ctrl.read_transfer_complete().await?;

    // Disconnect gracefully
    ctrl.quit().await.ok();

    // Calculate final statistics
    let final_speed = {
        let elapsed = start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            (cmd.completed_bytes as f64 / elapsed) as u64
        } else {
            0
        }
    };

    // Update final status in request group
    {
        let g = cmd.group.recover();
        g.update_progress(cmd.completed_bytes);
        g.update_speed(final_speed, 0);
        drop(g);
        let mut g = cmd.group.recover_mut();
        g.complete()?;
    }

    Ok(())
}
