//! Session management for download persistence
//!
//! This module handles saving and restoring download sessions:
//! - Restoring incomplete downloads from session files
//! - Saving session state on shutdown
//! - Mapping session entries to download options

use super::App;
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::session::active_session::ActiveSessionManager;
use aria2_core::util::rwlock_ext::RwLockRecover;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, warn};

impl App {
    /// Restore incomplete download tasks from a session file.
    ///
    /// This method is called at startup to resume downloads from the
    /// --input-file session file.
    ///
    /// # Restore Logic
    /// 1. Skip entries with status "complete"
    /// 2. Skip entries with both completed_length and total_length as 0
    /// 3. Recreate download tasks for entries with progress
    /// 4. BT download bitfield info is preserved for later use
    ///
    /// # Returns
    /// - `Ok(usize)`: Number of successfully restored tasks
    /// - `Err(String)`: Error during restoration
    pub async fn restore_session(&self) -> std::result::Result<usize, String> {
        let input_file = match self.get_opt_str("input-file").await {
            Some(path) => path,
            None => return Ok(0), // No input-file specified
        };

        let session_path = PathBuf::from(&input_file);
        if !session_path.exists() {
            info!(
                "Session file does not exist, skipping restore: {}",
                input_file
            );
            return Ok(0);
        }

        info!("Restoring download tasks from session file: {}", input_file);

        let mgr = ActiveSessionManager::new(
            session_path.clone(),
            Duration::from_secs(60), // Default interval, not used during restore
        );

        let entries = match mgr.load_session().await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to load session file: {}", e);
                return Err(e);
            }
        };

        if entries.is_empty() {
            info!("Session file is empty or has no recoverable entries");
            return Ok(0);
        }

        let mut restored_count = 0;

        for entry in &entries {
            // Skip completed entries
            if entry.status == "complete" {
                debug!("Skipping completed entry: GID={:x}", entry.gid);
                continue;
            }

            // C++ restores ALL non-finished entries, even those with 0/0 progress
            // (newly added but never started). Only skip entries that have
            // explicitly been marked as "removed" — they should not be restored.
            if entry.status == "removed" {
                debug!("Skipping removed entry: GID={:x}", entry.gid);
                continue;
            }

            // Map SessionEntry options to DownloadOptions
            let opts = Self::map_entry_to_download_options(&entry.options);

            info!(
                "Restoring download task: GID={:x}, URIs={:?}, progress={}/{}",
                entry.gid, entry.uris, entry.completed_length, entry.total_length
            );

            // Add group through RequestGroupMan
            {
                let man = self.request_man.read().await;
                match man.add_group(entry.uris.clone(), opts) {
                    Ok(gid) => {
                        restored_count += 1;
                        info!("Successfully restored task #{}", gid.value());

                        // Store BT bitfield if present
                        if entry.bitfield.is_some()
                            && let Some(group_lock) = man.get_group(gid)
                        {
                            let group = group_lock.recover_mut();
                            *group.bt_bitfield.recover_mut() = entry.bitfield.clone();
                            debug!(
                                "Set BT bitfield for GID={}, bits={}",
                                gid.value(),
                                entry.bitfield.as_ref().map(|b| b.len()).unwrap_or(0)
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Failed to restore task (GID={:x}): {}", entry.gid, e);
                    }
                }
            }
        }

        info!(
            "Session restore complete: {} entries total, {} tasks restored",
            entries.len(),
            restored_count
        );
        Ok(restored_count)
    }

    /// Save active session on application shutdown.
    ///
    /// Called after engine finishes to save all incomplete downloads
    /// to the session file.
    ///
    /// # Returns
    /// - `Ok(Option<usize>)`: Number of saved entries (if save-session is configured)
    /// - `Err(String)`: Error during save
    pub async fn save_session_on_shutdown(&self) -> std::result::Result<Option<usize>, String> {
        let save_path = match self.get_opt_str("save-session").await {
            Some(path) => path,
            None => {
                debug!("save-session not configured, skipping shutdown save");
                return Ok(None);
            }
        };

        info!("Saving session to: {}", save_path);

        let session_path = PathBuf::from(&save_path);
        let interval = self
            .get_opt_i64("save-session-interval")
            .await
            .unwrap_or(crate::constants::DEFAULT_SAVE_SESSION_INTERVAL_SECS as i64)
            .max(crate::constants::MIN_SESSION_INTERVAL_SECS as i64); // At least 1 second

        let mgr = ActiveSessionManager::new(session_path, Duration::from_secs(interval as u64));

        // Get all active groups
        let man = self.request_man.read().await;
        let groups = man.list_groups();

        if groups.is_empty() {
            info!("No active download tasks, skipping session save");
            return Ok(Some(0));
        }

        match mgr.save_session(&groups).await {
            Ok(n) => {
                info!("Successfully saved {} entries to {}", n, save_path);
                Ok(Some(n))
            }
            Err(e) => {
                warn!("Failed to save session: {}", e);
                Err(e)
            }
        }
    }

    /// Map SessionEntry options HashMap to DownloadOptions
    pub(super) fn map_entry_to_download_options(
        options: &std::collections::HashMap<String, String>,
    ) -> DownloadOptions {
        DownloadOptions {
            split: options.get("split").and_then(|v| v.parse::<u16>().ok()),
            max_connection_per_server: options
                .get("max-connection-per-server")
                .and_then(|v| v.parse::<u16>().ok()),
            max_download_limit: options
                .get("max-download-limit")
                .and_then(|v| v.parse::<u64>().ok()),
            max_upload_limit: options
                .get("max-upload-limit")
                .and_then(|v| v.parse::<u64>().ok()),
            dir: options.get("dir").cloned(),
            out: options.get("out").cloned(),
            seed_time: options.get("seed-time").and_then(|v| v.parse::<f64>().ok()),
            seed_ratio: options
                .get("seed-ratio")
                .and_then(|v| v.parse::<f64>().ok()),
            checksum: options.get("checksum").and_then(|v| {
                if let Some((algo, val)) = v.split_once('=') {
                    Some((algo.trim().to_string(), val.trim().to_string()))
                } else {
                    None
                }
            }),
            cookie_file: options.get("cookie-file").cloned(),
            cookies: options.get("cookies").cloned(),
            bt_force_encrypt: options
                .get("bt-force-encrypt")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_require_crypto: options
                .get("bt-require-crypto")
                .map(|v| v == "true")
                .unwrap_or(false),
            enable_dht: options
                .get("enable-dht")
                .map(|v| v != "false")
                .unwrap_or(true),
            dht_listen_port: options
                .get("dht-listen-port")
                .and_then(|v| v.parse::<u16>().ok()),
            dht_entry_point: options.get("dht-entry-point").and_then(|v| {
                if v.is_empty() {
                    None
                } else {
                    Some(
                        v.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    )
                }
            }),
            enable_public_trackers: options
                .get("enable-public-trackers")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_piece_selection_strategy: options
                .get("bt-piece-selection-strategy")
                .cloned()
                .unwrap_or_else(|| crate::constants::DEFAULT_PIECE_STRATEGY.to_string()),
            bt_endgame_threshold: options
                .get("bt-endgame-threshold")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(crate::constants::DEFAULT_BT_ENDGAME_THRESHOLD as u32),
            max_retries: options
                .get("max-retries")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(crate::constants::DEFAULT_MAX_RETRIES),
            retry_wait: options
                .get("retry-wait")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(crate::constants::DEFAULT_RETRY_WAIT_SECS),
            http_proxy: options.get("http-proxy").cloned(),
            all_proxy: options.get("all-proxy").cloned(),
            https_proxy: options.get("https-proxy").cloned(),
            ftp_proxy: options.get("ftp-proxy").cloned(),
            no_proxy: options.get("no-proxy").cloned(),
            dht_file_path: options.get("dht-file-path").cloned(),
            bt_max_upload_slots: options
                .get("bt-max-upload-slots")
                .and_then(|v| v.parse::<u32>().ok()),
            bt_optimistic_unchoke_interval: options
                .get("bt-optimistic-unchoke-interval")
                .and_then(|v| v.parse::<u64>().ok()),
            bt_snubbed_timeout: options
                .get("bt-snubbed-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            // G2: Piece selection priority mode
            bt_prioritize_piece: options
                .get("bt-prioritize-piece")
                .cloned()
                .unwrap_or_else(|| crate::constants::DEFAULT_PIECE_PRIORITY.to_string()),
            // uTP (UDP Transport Protocol - BEP 29)
            enable_utp: options
                .get("enable-utp")
                .map(|v| v == "true")
                .unwrap_or(false),
            utp_listen_port: options
                .get("utp-listen-port")
                .and_then(|v| v.parse::<u16>().ok()),
            // HTTP headers
            header: options
                .get("header")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            user_agent: options.get("user-agent").cloned(),
            referer: options.get("referer").cloned(),
            file_allocation: options.get("file-allocation").cloned(),
            mmap_threshold: options
                .get("mmap-threshold")
                .and_then(|v| v.parse::<u64>().ok()),
            secure_falloc: options
                .get("secure-falloc")
                .map(|v| v == "true")
                .unwrap_or(false),
            // Metalink
            metalink_location: options.get("metalink-location").cloned(),
            metalink_preferred_protocol: options.get("metalink-preferred-protocol").cloned(),
            select_file: options.get("select-file").cloned(),
            piece_length: options
                .get("piece-length")
                .and_then(|v| v.parse::<u64>().ok()),
            metalink_enable_unique_protocol: options
                .get("metalink-enable-unique-protocol")
                .map(|v| v != "false")
                .unwrap_or(true),
            // FTP
            connect_timeout: options
                .get("connect-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            startup_idle_time: options
                .get("startup-idle-time")
                .and_then(|v| v.parse::<u64>().ok()),
            lowest_speed_limit: options
                .get("lowest-speed-limit")
                .and_then(|v| v.parse::<u64>().ok()),
            ftp_pasv: options
                .get("ftp-pasv")
                .map(|v| v != "false")
                .unwrap_or(true),
            remote_time: options
                .get("remote-time")
                .map(|v| v == "true")
                .unwrap_or(false),
            dry_run: options.get("dry-run").map(|v| v == "true").unwrap_or(false),
            ftp_reuse_connection: options
                .get("ftp-reuse-connection")
                .map(|v| v != "false")
                .unwrap_or(true),
            // Download
            realtime_chunk_checksum: options
                .get("realtime-chunk-checksum")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_stop_timeout: options
                .get("bt-stop-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            // BitTorrent extended
            disable_ipv6: options
                .get("disable-ipv6")
                .map(|v| v == "true")
                .unwrap_or(false),
            listen_port: options.get("listen-port").cloned(),
            bt_enable_lpd: options
                .get("bt-enable-lpd")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_lpd_interface: options.get("bt-lpd-interface").cloned(),
            enable_rpc: options
                .get("enable-rpc")
                .map(|v| v == "true")
                .unwrap_or(false),
            pause: options.get("pause").map(|v| v == "true").unwrap_or(false),
        }
    }
}
