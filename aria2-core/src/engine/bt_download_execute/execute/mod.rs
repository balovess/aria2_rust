mod bep6;
mod dht_periodic_lookup;
mod finalization;
mod peer_management;
mod pex;
mod piece_download;
mod web_seed;

pub use dht_periodic_lookup::{DhtPeriodicLookup, check_periodic_dht_lookup};

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::types::PeerKey;
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

#[async_trait]
impl Command for BtDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
            self.started_at = Some(Instant::now());
        }

        // Register this BT download into the engine's BtRegistry so that
        // info-hash reverse lookup, peer blocklist, and cross-download BT
        // coordination work end-to-end. In C++ aria2, this is done by
        // `BtSetup::setup()` which calls `BtRegistry::put(gid, btObject)`.
        if let Some(ref registry) = self.bt_registry {
            let gid = self.group.recover().gid().value();
            let download_context = self.group.recover().get_download_context();

            // Build BtAnnounce from the torrent's announce list / announce URL.
            // In C++ aria2, BtSetup creates BtAnnounce from TorrentAttribute's
            // announce list and passes it to BtRegistry.
            let (announce_list, announce_url) = {
                use crate::download::download_context::{ContextAttributeType, TorrentAttribute};
                if let Some(ref ctx) = download_context {
                    if let Some(attr) = ctx.get_attribute(ContextAttributeType::BitTorrent) {
                        if let Some(ta) = attr.downcast_ref::<TorrentAttribute>() {
                            let list = &ta.announce_list;
                            let url = ta
                                .announce_list
                                .first()
                                .and_then(|tier| tier.first())
                                .cloned();
                            (list.clone(), url)
                        } else {
                            (Vec::new(), None)
                        }
                    } else {
                        (Vec::new(), None)
                    }
                } else {
                    (Vec::new(), None)
                }
            };
            let bt_announce = Arc::new(crate::engine::bt_tracker_comm::BtAnnounce::new(
                &announce_list,
                &announce_url,
            ));
            let bt_object = crate::engine::bt_registry::BtObject::builder()
                .download_context(download_context.unwrap_or_else(|| {
                    Arc::new(crate::download::DownloadContext::new(0, 0, String::new()))
                }))
                .peer_rejection(self.peer_rejection.clone())
                .bt_announce(bt_announce)
                .build();
            if let Ok(mut reg) = registry.write() {
                reg.put(gid, bt_object);
                info!(
                    gid,
                    "Registered BT download into BtRegistry with BtAnnounce"
                );
            }
        }

        let (meta, piece_length, total_size, num_pieces) = self.prepare_environment().await?;
        self.group
            .recover()
            .set_control_file_path(ControlFile::control_path_for(&self.output_path));

        // --check-integrity: verify existing data against the torrent's piece
        // hashes before allocating/downloading (mirrors C++
        // CheckIntegrityMan + CheckIntegrityCommand).
        let mut verified_piece_indices = Vec::new();
        if self.check_integrity {
            use crate::checksum::check_integrity::man as ci_man;
            use crate::checksum::message_digest::HashType;
            use crate::util::rwlock_ext::RwLockRecover;
            let gid = self.group.recover().gid().value();
            let piece_hashes_hex: Vec<String> = meta.info.pieces.iter().map(hex::encode).collect();
            let task = if let Some(ref layout) = self.multi_file_layout {
                let files: Vec<_> = layout
                    .file_list()
                    .iter()
                    .filter_map(|entry| {
                        layout
                            .file_absolute_path(entry.index)
                            .map(|path| (path.to_path_buf(), entry.length))
                    })
                    .collect();
                ci_man::cut_multi_file_trailing_garbage(&files).await?;
                ci_man::multi_file_task(
                    files,
                    piece_length as u64,
                    total_size,
                    piece_hashes_hex,
                    HashType::Sha1,
                )?
            } else {
                ci_man::cut_trailing_garbage(&self.output_path, total_size).await?;
                ci_man::file_task(
                    &self.output_path,
                    piece_length as u64,
                    total_size,
                    piece_hashes_hex,
                    HashType::Sha1,
                )?
            };
            if let Some(task) = task {
                info!(
                    gid,
                    "Checking integrity of existing data against piece hashes"
                );
                let outcome = ci_man::enqueue_with_outcome(&ci_man::shared(), gid, task).await?;
                verified_piece_indices = outcome.verified_piece_indices;
                if !outcome.failed_piece_indices.is_empty() {
                    warn!(
                        gid,
                        failed_pieces = ?outcome.failed_piece_indices,
                        "Integrity check found pieces to re-download"
                    );
                }
                // Only verified pieces enter the picker as complete. Failed
                // pieces are intentionally left missing, which makes the
                // runtime piece manager request them again rather than relying
                // on stale control-file state.
                info!(
                    gid,
                    verified_pieces = verified_piece_indices.len(),
                    "Integrity check completed, proceeding with download"
                );
            }
        }

        if self.hash_check_only {
            info!("hash-check-only enabled; stopping after integrity validation");
            if self.check_integrity && verified_piece_indices.len() == num_pieces as usize {
                self.completed_bytes = total_size;
                self.progress.set_completed_length(total_size);
                self.group.recover_mut().complete()?;
                info!("hash-check-only completed successfully");
                return Ok(());
            }
            return Err(Aria2Error::Fatal(FatalError::Config(
                "hash-check-only: existing data failed torrent piece hash validation".into(),
            )));
        }

        // File pre-allocation (mirrors C++ BtFileAllocationEntry queued into
        // FileAllocationMan after integrity checking). Single-file torrents
        // allocate `output_path`; multi-file torrents allocate every file in
        // layout order. Already-completed files are skipped by the worker.
        // The worker runs chunked, cooperative allocation sequentially across
        // downloads, so a huge zero-fill never blocks this task or the engine.
        {
            use crate::filesystem::file_allocation::AllocationStrategy;
            use crate::filesystem::file_allocation_man;
            let strategy = AllocationStrategy::from_str(&self.file_allocation);
            if strategy != AllocationStrategy::None {
                let gid = self.group.recover().gid().value();
                let man = file_allocation_man::shared();
                if let Some(ref layout) = self.multi_file_layout {
                    let files: Vec<(std::path::PathBuf, u64)> = layout
                        .file_list()
                        .iter()
                        .filter_map(|f| {
                            layout
                                .file_absolute_path(f.index)
                                .map(|p| (p.to_path_buf(), f.length))
                        })
                        .collect();
                    file_allocation_man::enqueue_multi(
                        &man,
                        files,
                        strategy,
                        self.secure_falloc,
                        gid,
                    )
                    .await?;
                } else {
                    file_allocation_man::enqueue_path(
                        &man,
                        &self.output_path,
                        total_size,
                        strategy,
                        self.secure_falloc,
                        gid,
                    )
                    .await?;
                }
            }
        }

        // P1 integration: try to resume from saved .aria2 progress file
        if let Some(ref mgr) = self.progress_manager {
            match mgr.load_progress(&meta.info_hash.bytes) {
                Ok(saved) => {
                    info!(
                        pieces_done = saved.num_pieces,
                        ratio = saved.completion_ratio(),
                        "Resuming from saved progress"
                    );
                }
                Err(e) => {
                    debug!(
                        error = %e,
                        "No saved progress found, starting fresh download"
                    );
                }
            }
        }

        let peer_addrs = self
            .discover_peers(&meta, total_size, &meta.info_hash.bytes)
            .await?;

        if peer_addrs.is_empty() {
            return Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: "No peers from tracker or DHT".into(),
                },
            ));
        }

        let mut active_connections = self
            .connect_to_peers(
                &peer_addrs,
                &meta.info_hash.bytes,
                num_pieces,
                piece_length,
                total_size,
            )
            .await?;

        // Initialize PEX known peers list from discovered peers for BEP 11 exchange.
        // BEP 0027 (Private Torrent): PEX must be disabled for private torrents
        // because it exchanges peer lists with connected peers, which would leak
        // the swarm membership beyond the tracker-controlled peer set.
        if self.is_private {
            info!("[BT] Private torrent: PEX disabled (BEP 0027)");
        } else {
            let pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> = peer_addrs
                .iter()
                .map(|pa| {
                    aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&pa.ip, pa.port)
                })
                .collect();
            self.set_pex_known_peers(pex_peers);
            info!(
                "[PEX] Initialized with {} known peers from tracker/DHT",
                self.pex_known_peers.len()
            );
        }

        // Initialize web seed manager if web seeds are available (BEP 19)
        let web_seed_manager = if !self.web_seed_urls.is_empty() {
            info!(
                "[BT] Initializing web seed manager with {} URL(s)",
                self.web_seed_urls.len()
            );
            Some(crate::engine::bt_web_seed::WebSeedManager::new(
                self.web_seed_urls.clone(),
                piece_length,
                total_size,
            ))
        } else {
            None
        };

        // Initialize PEX state only for peers whose BEP 10 handshake advertised
        // ut_pex. Private torrents keep this set empty per BEP 0027.
        let mut pex_enabled_peers: HashSet<PeerKey> = HashSet::new();
        let mut last_pex_send = Instant::now();
        const PEX_SEND_INTERVAL_SECS: u64 = 60;

        if !self.is_private {
            // PEX is enabled only after the remote BEP 10 handshake advertises
            // ut_pex. Each peer has an independent extension-ID namespace.
            for conn in active_connections.iter() {
                if conn.peer_extension_id("ut_pex").is_some()
                    && let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port)
                {
                    pex_enabled_peers.insert(peer_key);
                }
            }
            info!(
                "[PEX] Initialized PEX tracking for {} negotiated peers",
                pex_enabled_peers.len()
            );
        }

        // BEP 6 (Fast Extension): Send initial AllowedFast messages to
        // peers that support fast extension. The fast-set is computed from
        // the peer's IP address (see compute_fast_set in fast_set.rs).
        // Also, for peers that support fast extension, we can send HaveAll
        // or HaveNone instead of a full Bitfield when appropriate.
        for (idx, conn) in active_connections.iter_mut().enumerate() {
            if conn.is_fast_extension_enabled() {
                let mut sent = HashSet::new();
                match Self::send_allowed_fast_for_torrent(
                    conn,
                    num_pieces,
                    &meta.info_hash.bytes,
                    &mut sent,
                )
                .await
                {
                    Ok(count) if count > 0 => {
                        if let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) {
                            self.allowed_fast_sent_peers.insert(peer_key, sent);
                        }
                        debug!(
                            "[BEP6] Sent {} AllowedFast pieces to peer {} ({})",
                            count, idx, conn.ip_addr
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("[BEP6] Failed to flush AllowedFast to peer {}: {}", idx, e);
                    }
                }
            }
        }

        // Download pieces from the connected peers, using web seeds and PEX as configured.
        let piece_result = self
            .download_pieces_loop(
                &mut active_connections,
                &meta,
                piece_length,
                total_size,
                num_pieces,
                web_seed_manager.as_ref(),
                &mut pex_enabled_peers,
                &mut last_pex_send,
                PEX_SEND_INTERVAL_SECS,
                &verified_piece_indices,
            )
            .await;
        self.group.recover().clear_bt_peer_snapshots();
        piece_result?;

        if let Some(ref mut announcer) = self.tracker_announcer {
            announcer
                .announce_completed(
                    &meta.info_hash.bytes,
                    &self.local_peer_id,
                    self.completed_bytes,
                    self.total_uploaded,
                )
                .await;
        }

        if self.seed_enabled && !active_connections.is_empty() {
            info!(
                "Starting seeding phase with {} peers...",
                active_connections.len()
            );
            self.run_seeding_phase(
                active_connections,
                piece_length,
                num_pieces,
                meta.info_hash.bytes,
            )
            .await?;
        } else {
            info!(
                "Skipping seeding (enabled={}, connections={})",
                self.seed_enabled,
                active_connections.len()
            );
            for conn in &mut active_connections {
                let _ = conn;
            }
        }

        let started_at = self.started_at.unwrap_or_else(Instant::now);
        self.finalize_download(started_at, &meta).await?;

        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
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
        Some(Duration::from_secs(600))
    }
}

impl BtDownloadCommand {
    /// Prepare the download environment: create output directories, parse torrent
    /// metadata, and set total length on the request group.
    async fn prepare_environment(
        &mut self,
    ) -> Result<(
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        u32,
        u64,
        u32,
    )> {
        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        if let Some(ref layout) = self.multi_file_layout {
            layout.create_directories().map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "create_directories failed: {}",
                    e
                )))
            })?;
            info!(
                "[BT] Multi-file mode: {} files under {}",
                layout.num_files(),
                self.output_path.display()
            );
        }

        let meta =
            aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&self.torrent_data)
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("Torrent parse error: {}", e)))
                })?;

        {
            let g = self.group.recover();
            g.set_total_length(meta.total_size());
        }

        let piece_length = meta.info.piece_length;
        let total_size = meta.total_size();
        let num_pieces = meta.num_pieces() as u32;

        Ok((meta, piece_length, total_size, num_pieces))
    }
}
