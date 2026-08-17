mod bep6;
mod dht_periodic_lookup;
mod finalization;
mod peer_management;
mod pex;
mod piece_download;
mod web_seed;

pub use dht_periodic_lookup::{DhtPeriodicLookup, check_periodic_dht_lookup};

pub(crate) fn deduplicate_tracker_tiers(tiers: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let mut seen = HashSet::new();
    tiers
        .into_iter()
        .filter_map(|tier| {
            let unique = tier
                .into_iter()
                .filter(|url| seen.insert(url.clone()))
                .collect::<Vec<_>>();
            (!unique.is_empty()).then_some(unique)
        })
        .collect()
}

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::types::PeerKey;
use crate::config::parse_integer_segments;
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::http::client_identity::ClientTlsConfig;
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    pub(super) async fn persist_checkpoint_after_piece(
        &mut self,
        writer: &mut Box<dyn crate::filesystem::disk_writer::SeekableDiskWriter>,
        bitfield: &[u8],
    ) -> Result<()> {
        let save_requested = self.group.recover().is_save_control_file_requested();
        if save_requested {
            writer.flush().await.map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to flush requested BitTorrent checkpoint: {error}"
                ))
            })?;
        }

        let Some(checkpoint) = self.checkpoint.as_mut() else {
            if save_requested {
                return Err(Aria2Error::FileIo(
                    "Requested BitTorrent checkpoint is unavailable".into(),
                ));
            }
            return Ok(());
        };

        match checkpoint.save(bitfield, self.completed_bytes).await {
            Ok(()) => {
                if save_requested {
                    self.group.recover().take_save_control_file_request();
                }
                Ok(())
            }
            Err(error) if save_requested => Err(Aria2Error::FileIo(format!(
                "Failed to save requested BitTorrent checkpoint: {error}"
            ))),
            Err(error) => {
                warn!(%error, "Failed to save BT checkpoint after piece completion");
                Ok(())
            }
        }
    }

    fn drain_incoming_peers(
        &mut self,
        active_connections: &mut Vec<crate::engine::bt_peer_connection::BtPeerConn>,
        piece_length: u32,
        total_size: u64,
    ) {
        let Some(receiver) = self.incoming_peers.as_mut() else {
            return;
        };
        while let Ok(incoming) = receiver.try_recv() {
            let endpoint = incoming.endpoint;
            let mut conn = match incoming.connection {
                aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Plain(
                    connection,
                ) => crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
                    *connection,
                    endpoint,
                ),
                aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Encrypted(
                    connection,
                ) => crate::engine::bt_peer_connection::BtPeerConn::from_incoming_encrypted(
                    *connection,
                    endpoint,
                ),
            };
            let remote_peer_id = conn.remote_peer_id();
            if remote_peer_id == Some(self.local_peer_id)
                || remote_peer_id.is_some_and(|peer_id| {
                    active_connections
                        .iter()
                        .any(|active| active.peer_id == Some(peer_id))
                })
            {
                self.peer_storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
                info!(%endpoint, "Rejected incoming self or duplicate BitTorrent peer");
                continue;
            }
            conn.allocate_session_resource(piece_length, total_size);
            active_connections.push(conn);
            self.bt_runtime.set_connections(active_connections.len());
            info!("[BT] Admitted incoming peer {}", endpoint);
        }
    }
}

#[async_trait]
impl Command for BtDownloadCommand {
    async fn shutdown(&mut self) {
        self.dht_periodic_lookup.cancel_pending_lookup().await;
        BtDownloadCommand::shutdown(self).await;
    }

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
                .bt_announce(bt_announce)
                .download_context(download_context.unwrap_or_else(|| {
                    Arc::new(crate::download::DownloadContext::new(0, 0, String::new()))
                }))
                .peer_rejection(self.peer_rejection.clone())
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

        // A zero-length torrent has no pieces or peers to acquire. Complete
        // it after metadata preparation, matching the normal download
        // lifecycle without entering tracker/peer discovery.
        if total_size == 0 {
            self.completed_bytes = 0;
            self.progress.set_completed_length(0);
            let payload_exists = self.bt_payload_exists();
            let checkpoint = crate::engine::bt_checkpoint::BtCheckpoint::open(
                &self.output_path,
                payload_exists,
                total_size,
                piece_length,
                num_pieces as usize,
                meta.info_hash.bytes,
            )
            .await?;
            checkpoint.remove().await?;
            self.group.recover_mut().complete()?;
            info!("BT zero-length download completed without peer discovery");
            return Ok(());
        }

        let payload_exists = self.bt_payload_exists();
        let checkpoint = crate::engine::bt_checkpoint::BtCheckpoint::open(
            &self.output_path,
            payload_exists,
            total_size,
            piece_length,
            num_pieces as usize,
            meta.info_hash.bytes,
        )
        .await?;
        self.completed_bytes = if self.check_integrity {
            0
        } else {
            checkpoint.completed_length()
        };
        self.progress.set_completed_length(self.completed_bytes);
        self.group
            .recover()
            .set_bt_bitfield(checkpoint.bitfield().map(ToOwned::to_owned));
        self.checkpoint = Some(checkpoint);

        // C++ `--bt-seed-unverified` marks an existing payload complete before
        // the integrity command is scheduled. Keep hash-check-only explicit:
        // it is a diagnostic request and must still validate the payload.
        let seed_unverified = self.bt_seed_unverified && payload_exists && !self.hash_check_only;
        let mut verified_piece_indices = if seed_unverified {
            info!(
                "bt-seed-unverified enabled; treating existing payload as complete without piece-hash validation"
            );
            self.completed_bytes = total_size;
            self.progress.set_completed_length(total_size);
            (0..num_pieces as usize).collect()
        } else {
            Vec::new()
        };

        // --check-integrity: verify existing data against the torrent's piece
        // hashes before allocating/downloading (mirrors C++
        // CheckIntegrityMan + CheckIntegrityCommand).
        let mut integrity_finished_action =
            crate::checksum::check_integrity::IntegrityFinishedAction::default();
        if self.check_integrity && !seed_unverified {
            use crate::checksum::check_integrity::{IntegrityTrailingGarbageAction, man as ci_man};
            use crate::checksum::message_digest::HashType;
            use crate::util::rwlock_ext::RwLockRecover;
            let gid = self.group.recover().gid().value();
            let piece_hashes_hex: Vec<String> = meta.info.pieces.iter().map(hex::encode).collect();
            let integrity_files = self.integrity_files(total_size);
            IntegrityTrailingGarbageAction::new(integrity_files.clone())
                .apply()
                .await?;
            let task = if self.multi_file_layout.is_some() {
                ci_man::multi_file_task(
                    integrity_files
                        .iter()
                        .map(|file| (file.path.clone(), file.length))
                        .collect(),
                    piece_length as u64,
                    total_size,
                    piece_hashes_hex,
                    HashType::Sha1,
                )?
            } else {
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
                integrity_finished_action =
                    crate::checksum::check_integrity::IntegrityFinishedAction::for_bt(
                        integrity_files,
                        self.hash_check_only,
                        self.bt_hash_check_seed,
                        self.bt_enable_hook_after_hash_check,
                    );
                self.completed_bytes = verified_piece_indices
                    .iter()
                    .filter_map(|&index| {
                        (index < num_pieces as usize).then_some(
                            total_size
                                .saturating_sub(index as u64 * piece_length as u64)
                                .min(piece_length as u64),
                        )
                    })
                    .sum();
                self.progress.set_completed_length(self.completed_bytes);
                if let Some(checkpoint) = self.checkpoint.as_mut()
                    && let Err(error) = checkpoint
                        .save_verified_pieces(
                            verified_piece_indices.iter().copied(),
                            self.completed_bytes,
                        )
                        .await
                {
                    warn!(%error, "Failed to rewrite BT checkpoint after integrity checking");
                }

                // A complete integrity check is a distinct lifecycle from a
                // normal piece download. The original public contract emits
                // the BT completion hook at this seam and only continues
                // into peer/seed setup when bt-hash-check-seed is enabled.
                if verified_piece_indices.len() == num_pieces as usize && !self.hash_check_only {
                    self.completed_bytes = total_size;
                    self.progress.set_completed_length(total_size);
                    self.hash_check_completed = true;
                    self.bt_complete_event_emitted = true;
                    if integrity_finished_action.run_completion_hook {
                        crate::engine::download_event_hooks::DownloadEventHooks::shared()
                            .fire_event(
                                crate::engine::download_event_hooks::DownloadEvent::BtComplete,
                                &self.group.recover(),
                            );
                    }
                }
            }
        }

        if self.hash_check_only {
            info!("hash-check-only enabled; stopping after integrity validation");
            if self.check_integrity && verified_piece_indices.len() == num_pieces as usize {
                self.completed_bytes = total_size;
                self.progress.set_completed_length(total_size);
                if let Some(checkpoint) = self.checkpoint.take() {
                    checkpoint.remove().await?;
                }
                self.group.recover_mut().complete()?;
                info!("hash-check-only completed successfully");
                return Ok(());
            }
            return Err(Aria2Error::Fatal(FatalError::Config(
                "hash-check-only: existing data failed torrent piece hash validation".into(),
            )));
        }

        // A successful integrity check with seeding disabled is already a
        // complete download. Finish locally instead of discovering peers or
        // announcing a new torrent session.
        if self.hash_check_completed && !self.bt_hash_check_seed {
            info!(
                "Integrity check completed; bt-hash-check-seed disabled, stopping without seeding"
            );
            let started_at = self.started_at.unwrap_or_else(Instant::now);
            self.finalize_download(started_at, &meta).await?;
            return Ok(());
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
                let allocation_files = if self.hash_check_completed {
                    integrity_finished_action
                        .file_allocation
                        .clone()
                        .unwrap_or_else(|| self.integrity_files(total_size))
                } else {
                    self.integrity_files(total_size)
                };
                if self.multi_file_layout.is_some() {
                    let files: Vec<(std::path::PathBuf, u64)> = allocation_files
                        .into_iter()
                        .map(|file| (file.path, file.length))
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

        // BtSetup/PeerListenCommand counterpart: register this torrent on the
        // engine-owned listener before discovery so one socket can serve all
        // active torrents and route by info-hash.
        if self.incoming_peers.is_none() {
            let listener_manager = self.bt_listener.clone().ok_or_else(|| {
                Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: "BitTorrent listener manager is not configured".to_string(),
                })
            })?;
            let (listen_ports, max_peers, caretaker_id, disable_ipv6, crypto_policy) = {
                let group = self.group.recover();
                let ports = group
                    .options()
                    .listen_port
                    .as_deref()
                    .map(parse_listen_ports)
                    .transpose()
                    .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?
                    .unwrap_or_else(|| vec![0]);
                (
                    ports,
                    group.options().bt_max_peers,
                    group.gid().value(),
                    group.options().disable_ipv6,
                    aria2_protocol::bittorrent::peer::incoming::IncomingCryptoPolicy {
                        reject_plain: group.options().bt_force_encrypt
                            || group.options().bt_require_crypto,
                        force_encryption: group.options().bt_force_encrypt,
                        prefer_encryption: group
                            .effective_option_snapshot()
                            .and_then(|snapshot| {
                                snapshot
                                    .get("bt-min-crypto-level")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned)
                            })
                            .is_some_and(|level| level.eq_ignore_ascii_case("arc4"))
                            || group.options().bt_force_encrypt,
                    },
                )
            };
            let register = |bind_ip: std::net::IpAddr| {
                listener_manager.register(crate::engine::bt_peer_listener::BtPeerRouteConfig {
                    bind_ip,
                    ports: listen_ports.clone(),
                    info_hash: meta.info_hash.bytes,
                    local_peer_id: self.local_peer_id,
                    caretaker_id,
                    max_peers,
                    peer_storage: std::sync::Arc::clone(&self.peer_storage),
                    crypto_policy,
                })
            };
            let route = if disable_ipv6 {
                register(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)).await
            } else {
                match register(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)).await {
                    Ok(route) => Ok(route),
                    Err(ipv6_error) => {
                        warn!(
                            error = %ipv6_error,
                            "IPv6 BitTorrent listener unavailable; falling back to IPv4"
                        );
                        register(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)).await
                    }
                }
            }
            .map_err(|error| {
                Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: format!("failed to bind BitTorrent peer listener: {error}"),
                })
            })?;
            let (listen_port, incoming_peers, route_handle) = route;
            self.listen_port = listen_port;
            if let Some(registry) = &self.bt_registry
                && let Ok(mut registry) = registry.write()
            {
                registry.set_tcp_port(self.listen_port);
            }
            self.incoming_peers = Some(incoming_peers);
            self.bt_peer_route = Some(route_handle);
            info!(
                "[BT] Incoming peer route registered on TCP port {}",
                listen_port
            );
        }

        let peer_addrs = self
            .discover_peers(&meta, total_size, &meta.info_hash.bytes)
            .await?;

        let mut active_connections = if peer_addrs.is_empty() {
            Vec::new()
        } else {
            self.connect_to_peers(
                &peer_addrs,
                &meta.info_hash.bytes,
                num_pieces,
                piece_length,
                total_size,
            )
            .await?
        };

        // The initial DHT lookup is complete once discovery and the first
        // PeerStorage admission have finished. Record the same count the
        // original DHTGetPeersCommand uses for retry decisions.
        if self.dht_engine.is_some() {
            self.dht_periodic_lookup
                .set_peer_limits(self.bt_runtime.min_peers(), self.bt_runtime.max_peers());
            self.dht_periodic_lookup
                .record_lookup_completed(self.tracked_peer_count());
        }

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
            let web_seed_tls = {
                let group = self.group.recover();
                ClientTlsConfig::from_download_options(group.options())
            };
            Some(
                crate::engine::bt_web_seed::WebSeedManager::new_with_tls(
                    self.web_seed_urls.clone(),
                    piece_length,
                    total_size,
                    &web_seed_tls,
                )
                .map_err(|error| {
                    Aria2Error::Fatal(FatalError::Config(format!(
                        "Web-seed HTTP client configuration failed: {error}"
                    )))
                })?,
            )
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

        // Admit handshaken incoming peers before the first piece cycle. Later
        // cycles drain the receiver below, preserving PeerListenCommand's
        // long-lived listener semantics.
        self.drain_incoming_peers(&mut active_connections, piece_length, total_size);

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
        if let Err(error) = piece_result {
            if let Some(ref mut announcer) = self.tracker_announcer {
                announcer
                    .announce_stopped(
                        &meta.info_hash.bytes,
                        &self.local_peer_id,
                        self.completed_bytes,
                        total_size.saturating_sub(self.completed_bytes),
                        self.total_uploaded,
                    )
                    .await;
            }
            return Err(error);
        }

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

        if self.seed_enabled {
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
            info!("Skipping seeding (enabled={})", self.seed_enabled,);
            for conn in &mut active_connections {
                let _ = conn;
            }
        }

        let started_at = self.started_at.unwrap_or_else(Instant::now);
        self.finalize_download(started_at, &meta).await?;

        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.group.recover().status() == crate::request::request_group::DownloadStatus::Complete
        {
            CommandStatus::Completed
        } else if self.completed_bytes > 0 {
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

fn parse_listen_ports(value: &str) -> std::result::Result<Vec<u16>, String> {
    let ports = parse_integer_segments(value, 1024, u16::MAX as i64)?
        .into_iter()
        .flat_map(|range| range.map(|port| port as u16))
        .collect::<Vec<_>>();
    Ok(ports)
}

impl BtDownloadCommand {
    /// Prepare the download environment: create output directories, parse torrent metadata, and set total length on the request group.
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

    fn integrity_files(
        &self,
        total_size: u64,
    ) -> Vec<crate::checksum::check_integrity::IntegrityFile> {
        match self.multi_file_layout.as_ref() {
            Some(layout) => layout
                .file_list()
                .iter()
                .filter_map(|entry| {
                    layout.file_absolute_path(entry.index).map(|path| {
                        crate::checksum::check_integrity::IntegrityFile::new(
                            path.to_path_buf(),
                            entry.length,
                        )
                    })
                })
                .collect(),
            None => vec![crate::checksum::check_integrity::IntegrityFile::new(
                self.output_path.clone(),
                total_size,
            )],
        }
    }

    fn bt_payload_exists(&self) -> bool {
        match self.multi_file_layout.as_ref() {
            Some(layout) => layout.file_list().into_iter().all(|entry| {
                layout
                    .file_absolute_path(entry.index)
                    .is_some_and(|path| entry.length == 0 || path.is_file())
            }),
            None => self.output_path.is_file(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_listen_ports;

    #[test]
    fn listen_port_parser_expands_original_segment_syntax() {
        assert_eq!(
            parse_listen_ports("6881-6883,6999").unwrap(),
            vec![6881, 6882, 6883, 6999]
        );
    }

    #[test]
    fn listen_port_parser_rejects_values_outside_original_bounds() {
        assert!(parse_listen_ports("1023").is_err());
        assert!(parse_listen_ports("70000").is_err());
        assert!(parse_listen_ports("6881-").is_err());
    }
}
