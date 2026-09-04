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
use sha1::Digest;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use super::types::PeerKey;
use crate::config::parse_integer_segments;
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_piece_selector::build_bitfield_from_completed;
use crate::engine::bt_progress_info_file::BtProgress;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::http::client_identity::ClientTlsConfig;
#[cfg(feature = "bittorrent")]
use crate::request::request_group::BtConnectionGuard;
use crate::request::request_group::{ActiveConnectionGuard, GroupId};
use crate::util::rwlock_ext::RwLockRecover;

const MAX_PIECE_HASH_WORKERS: usize = 4;

fn piece_hash_semaphore() -> &'static Arc<Semaphore> {
    static HASH_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    HASH_SLOTS.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, MAX_PIECE_HASH_WORKERS);
        Arc::new(Semaphore::new(workers))
    })
}

/// Verify a downloaded piece without running the digest on a Tokio worker.
///
/// The owned payload is returned so callers can write it after verification
/// without allocating a second piece-sized buffer.
pub(super) async fn verify_piece_hash_async(
    expected: Option<crate::engine::bt_piece::PieceVerification>,
    data: Vec<u8>,
) -> Result<(bool, Vec<u8>)> {
    let Some(expected) = expected else {
        return Ok((false, data));
    };
    let permit = piece_hash_semaphore()
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| Aria2Error::Io(format!("piece hash dispatcher closed: {error}")))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let verified = match expected {
            crate::engine::bt_piece::PieceVerification::Sha1(hashes) => hashes
                .first()
                .is_some_and(|hash| sha1::Sha1::digest(&data).as_slice() == hash),
            crate::engine::bt_piece::PieceVerification::V2 {
                piece_length,
                hashes,
            } => hashes
                .first()
                .zip(aria2_protocol::bittorrent::torrent::merkle::piece_root(
                    &data,
                    piece_length as usize,
                ))
                .is_some_and(|(expected, actual)| expected.as_ref() == Some(&actual)),
            crate::engine::bt_piece::PieceVerification::Hybrid {
                piece_length,
                sha1,
                v2_hashes,
                v2_content_lengths,
            } => {
                let sha1_ok = sha1
                    .first()
                    .is_some_and(|hash| sha1::Sha1::digest(&data).as_slice() == hash);
                let v2_ok = match v2_hashes.first().and_then(Option::as_ref) {
                    Some(expected) => v2_content_lengths
                        .first()
                        .copied()
                        .filter(|length| *length != 0)
                        .and_then(|length| data.get(..length as usize))
                        .and_then(|content| {
                            aria2_protocol::bittorrent::torrent::merkle::piece_root(
                                content,
                                piece_length as usize,
                            )
                        })
                        .is_some_and(|actual| expected == &actual),
                    None => true,
                };
                sha1_ok && v2_ok
            }
        };
        (verified, data)
    })
    .await
    .map_err(|error| Aria2Error::Io(format!("piece hash task failed: {error}")))
}

fn checkpoint_save_due(
    save_requested: bool,
    bytes_since_save: u64,
    last_save: Instant,
    now: Instant,
) -> bool {
    save_requested
        || bytes_since_save >= crate::constants::BT_CHECKPOINT_SAVE_BYTES
        || now.saturating_duration_since(last_save)
            >= Duration::from_secs(crate::constants::BT_CHECKPOINT_SAVE_INTERVAL_SECS)
}

fn legacy_progress_piece_indices(
    progress: &BtProgress,
    piece_length: u32,
    total_size: u64,
    num_pieces: u32,
) -> Option<Vec<usize>> {
    let num_pieces_usize = num_pieces as usize;
    if !progress.is_torrent
        || progress.piece_length != piece_length
        || progress.total_size != total_size
        || progress.num_pieces != num_pieces
        || progress.bitfield.len() != num_pieces_usize.div_ceil(8)
    {
        return None;
    }

    let unused_bits = (8 - num_pieces_usize % 8) % 8;
    if unused_bits != 0
        && progress
            .bitfield
            .last()
            .is_none_or(|byte| byte & ((1u8 << unused_bits) - 1) != 0)
    {
        return None;
    }

    Some(
        (0..num_pieces_usize)
            .filter(|&index| {
                progress
                    .bitfield
                    .get(index / 8)
                    .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
            })
            .collect(),
    )
}

fn completed_piece_bytes(indices: &[usize], piece_length: u32, total_size: u64) -> u64 {
    indices
        .iter()
        .map(|&index| {
            total_size
                .saturating_sub(index as u64 * piece_length as u64)
                .min(piece_length as u64)
        })
        .sum()
}

fn initial_bt_progress(check_integrity: bool, checkpoint_completed_length: u64) -> (u64, u64) {
    let command_completed_length = if check_integrity {
        0
    } else {
        checkpoint_completed_length
    };
    (command_completed_length, checkpoint_completed_length)
}

impl BtDownloadCommand {
    pub(super) async fn persist_checkpoint_after_piece(
        &mut self,
        writer: &mut Box<dyn crate::filesystem::disk_writer::SeekableDiskWriter>,
        bitfield: &[u8],
        piece_bytes: u64,
    ) -> Result<()> {
        let save_requested = self.group.recover().is_save_control_file_requested();
        let Some(checkpoint) = self.checkpoint.as_mut() else {
            if save_requested {
                return Err(Aria2Error::FileIo(
                    "Requested BitTorrent checkpoint is unavailable".into(),
                ));
            }
            return Ok(());
        };

        self.checkpoint_bytes_since_save =
            self.checkpoint_bytes_since_save.saturating_add(piece_bytes);
        if !checkpoint_save_due(
            save_requested,
            self.checkpoint_bytes_since_save,
            self.checkpoint_last_save,
            Instant::now(),
        ) {
            return Ok(());
        }

        // The single-file BT writer uses a write-back cache. Persist payload
        // bytes before its bitfield so a restored checkpoint never advertises
        // a verified piece whose data is still only in memory.
        writer.flush().await.map_err(|error| {
            Aria2Error::FileIo(format!(
                "Failed to flush BitTorrent checkpoint payload: {error}"
            ))
        })?;

        let save_started = std::time::Instant::now();
        match checkpoint.save(bitfield, self.completed_bytes).await {
            Ok(()) => {
                self.checkpoint_bytes_since_save = 0;
                self.checkpoint_last_save = std::time::Instant::now();
                tracing::debug!(
                    piece_bytes,
                    save_ms = save_started.elapsed().as_millis() as u64,
                    forced = save_requested,
                    "BT checkpoint persisted"
                );
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
        let Some(mut receiver) = self.incoming_peers.take() else {
            return;
        };
        while let Ok(incoming) = receiver.try_recv() {
            self.admit_incoming_peer(active_connections, incoming, piece_length, total_size);
        }
        self.incoming_peers = Some(receiver);
    }

    pub(super) fn admit_incoming_peer(
        &mut self,
        active_connections: &mut Vec<crate::engine::bt_peer_connection::BtPeerConn>,
        incoming: crate::engine::bt_peer_listener::IncomingPeer,
        piece_length: u32,
        total_size: u64,
    ) {
        let endpoint = incoming.endpoint;
        let mut conn = match incoming.connection {
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Plain(connection) => {
                crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
                    *connection,
                    endpoint,
                )
            }
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Encrypted(
                connection,
            ) => crate::engine::bt_peer_connection::BtPeerConn::from_incoming_encrypted(
                *connection,
                endpoint,
            ),
        };
        self.apply_peer_exchange_policy(&mut conn);
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
            return;
        }
        if !self.should_admit_incoming_peer(active_connections.len()) {
            self.peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
            info!(
                %endpoint,
                "Rejected incoming BitTorrent peer because peer speed is above the request threshold"
            );
            return;
        }
        conn.allocate_session_resource(piece_length, total_size);
        active_connections.push(conn);
        self.bt_runtime.set_connections(active_connections.len());
        self.group
            .recover()
            .set_bt_connection_count(active_connections.len());
        info!("[BT] Admitted incoming peer {}", endpoint);
    }
}

#[async_trait]
impl Command for BtDownloadCommand {
    async fn shutdown(&mut self) {
        self.dht_periodic_lookup.cancel_pending_lookup().await;
        BtDownloadCommand::shutdown(self).await;
    }

    async fn execute(&mut self) -> Result<()> {
        let _connection_guard = ActiveConnectionGuard::new(Arc::clone(&self.group));
        #[cfg(feature = "bittorrent")]
        let _bt_connection_guard = BtConnectionGuard::new(Arc::clone(&self.group));
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

        let (mut meta, piece_length, total_size, num_pieces) = self.prepare_environment().await?;
        let network_info_hash = meta.network_info_hash();
        self.group
            .recover()
            .set_control_file_path(ControlFile::control_path_for(&self.output_path));

        // A zero-length torrent has no pieces or peers to acquire. Complete
        // it after metadata preparation, matching the normal download
        // lifecycle without entering tracker/peer discovery.
        if total_size == 0 {
            self.create_zero_length_payload().await?;
            self.completed_bytes = 0;
            self.progress.set_completed_length(0);
            let payload_exists = self.bt_payload_exists();
            let checkpoint = crate::engine::bt_checkpoint::BtCheckpoint::open(
                &self.output_path,
                payload_exists,
                total_size,
                piece_length,
                num_pieces as usize,
                network_info_hash,
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
            network_info_hash,
        )
        .await?;
        let checkpoint_completed_length = checkpoint.completed_length();
        let (command_completed_length, visible_completed_length) =
            initial_bt_progress(self.check_integrity, checkpoint_completed_length);
        self.completed_bytes = command_completed_length;
        // Integrity checking may take a long time for a large payload. Keep
        // the last durable piece progress visible while it runs; the
        // verified-piece result below replaces it if corruption is found.
        self.progress.set_completed_length(visible_completed_length);
        self.group
            .recover()
            .set_bt_bitfield(checkpoint.bitfield().map(ToOwned::to_owned));
        self.checkpoint = Some(checkpoint);
        self.checkpoint_bytes_since_save = 0;
        self.checkpoint_last_save = Instant::now();

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
                let outcome = ci_man::enqueue_with_outcome_for_group(
                    &ci_man::shared(),
                    Arc::clone(&self.group),
                    task,
                )
                .await?;
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

        // Integrity results and the explicit unverified-seed path both replace
        // the checkpoint's trust state. Publish the same verified-piece view
        // to RPC, CLI, and TUI before the command enters peer/seeding phases.
        if self.check_integrity || seed_unverified {
            let verified_piece_set: HashSet<usize> =
                verified_piece_indices.iter().copied().collect();
            let verified_bitfield = build_bitfield_from_completed(num_pieces, |index| {
                verified_piece_set.contains(&(index as usize))
            });
            self.group
                .recover()
                .set_bt_bitfield(Some(verified_bitfield));
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

        // P1 integration: use the C++-compatible progress file only as a
        // fallback when the Rust-owned A2CF has no progress. Integrity checks
        // remain authoritative because a progress file records trust, not
        // fresh hash evidence.
        if let Some(ref mgr) = self.progress_manager {
            match mgr.load_progress(&network_info_hash) {
                Ok(saved)
                    if !self.check_integrity
                        && !seed_unverified
                        && payload_exists
                        && self.completed_bytes == 0 =>
                {
                    match legacy_progress_piece_indices(
                        &saved,
                        piece_length,
                        total_size,
                        num_pieces,
                    ) {
                        Some(indices) if !indices.is_empty() => {
                            self.completed_bytes =
                                completed_piece_bytes(&indices, piece_length, total_size);
                            self.progress.set_completed_length(self.completed_bytes);
                            self.group
                                .recover()
                                .set_bt_bitfield(Some(saved.bitfield.clone()));
                            verified_piece_indices = indices;
                            info!(
                                pieces_done = verified_piece_indices.len(),
                                completed_bytes = self.completed_bytes,
                                "Resuming from legacy BT progress"
                            );
                        }
                        Some(_) => debug!("Saved BT progress has no completed pieces"),
                        None => warn!("Ignoring BT progress with incompatible torrent layout"),
                    }
                }
                Ok(_) => debug!(
                    "Ignoring saved BT progress because a newer checkpoint or integrity result is authoritative"
                ),
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
                            .options()
                            .bt_min_crypto_level
                            .eq_ignore_ascii_case("arc4")
                            || group.options().bt_force_encrypt,
                    },
                )
            };
            let register = |bind_ip: std::net::IpAddr| {
                listener_manager.register(crate::engine::bt_peer_listener::BtPeerRouteConfig {
                    bind_ip,
                    ports: listen_ports.clone(),
                    info_hash: network_info_hash,
                    info_hash_v2: meta.info_hash_v2,
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
            .discover_peers(&meta, total_size, &network_info_hash)
            .await?;

        let mut active_connections = if peer_addrs.is_empty() {
            Vec::new()
        } else {
            self.connect_to_peers(
                &peer_addrs,
                &network_info_hash,
                meta.info_hash_v2,
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

        // Initialize web seed manager only when the task explicitly enables
        // the BEP 19 fallback. The torrent's url-list is metadata, not an
        // instruction to bypass --bt-enable-web-seed=false.
        let web_seed_enabled = self.group.recover().options().bt_enable_web_seed;
        let web_seed_urls = self.configured_web_seed_urls();
        let web_seed_manager = if web_seed_enabled && !web_seed_urls.is_empty() {
            info!(
                "[BT] Initializing web seed manager with {} URL(s)",
                web_seed_urls.len()
            );
            let web_seed_tls = {
                let group = self.group.recover();
                ClientTlsConfig::from_download_options(group.options())
            };
            Some(
                crate::engine::bt_web_seed::WebSeedManager::new_with_tls(
                    web_seed_urls,
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

        if self.peer_exchange_enabled() {
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
                    &network_info_hash,
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
                &mut meta,
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
                        &network_info_hash,
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
                    &network_info_hash,
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
                network_info_hash,
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
        self.group.recover().timeout()
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

    async fn create_zero_length_payload(&self) -> Result<()> {
        let paths = match self.multi_file_layout.as_ref() {
            Some(layout) => (0..layout.num_files())
                .filter_map(|index| layout.file_absolute_path(index).map(PathBuf::from))
                .collect(),
            None => vec![self.output_path.clone()],
        };

        for path in paths {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    Aria2Error::FileCreate(format!(
                        "Failed to create directory '{}': {error}",
                        parent.display()
                    ))
                })?;
            }
            tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .await
                .map_err(|error| {
                    Aria2Error::FileCreate(format!(
                        "Failed to create zero-length payload '{}': {error}",
                        path.display()
                    ))
                })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        checkpoint_save_due, completed_piece_bytes, initial_bt_progress,
        legacy_progress_piece_indices, parse_listen_ports,
    };
    use crate::engine::bt_progress_info_file::BtProgress;
    use std::time::{Duration, Instant};

    #[test]
    fn checkpoint_save_due_honors_explicit_request_and_thresholds() {
        let start = Instant::now();
        assert!(checkpoint_save_due(true, 0, start, start));
        assert!(checkpoint_save_due(
            false,
            crate::constants::BT_CHECKPOINT_SAVE_BYTES,
            start,
            start
        ));
        assert!(checkpoint_save_due(
            false,
            0,
            start,
            start + Duration::from_secs(crate::constants::BT_CHECKPOINT_SAVE_INTERVAL_SECS)
        ));
        assert!(!checkpoint_save_due(false, 0, start, start));
    }

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

    #[test]
    fn legacy_progress_restores_only_a_matching_layout() {
        let progress = BtProgress {
            bitfield: vec![0b1010_0000],
            piece_length: 4,
            total_size: 10,
            num_pieces: 3,
            is_torrent: true,
            ..BtProgress::default()
        };

        let indices = legacy_progress_piece_indices(&progress, 4, 10, 3).unwrap();
        assert_eq!(indices, vec![0, 2]);
        assert_eq!(completed_piece_bytes(&indices, 4, 10), 6);
        assert!(legacy_progress_piece_indices(&progress, 5, 10, 3).is_none());
        assert!(legacy_progress_piece_indices(&progress, 4, 11, 3).is_none());
    }

    #[test]
    fn legacy_progress_rejects_set_trailing_bits() {
        let progress = BtProgress {
            bitfield: vec![0b1010_0001],
            piece_length: 4,
            total_size: 10,
            num_pieces: 3,
            is_torrent: true,
            ..BtProgress::default()
        };

        assert!(legacy_progress_piece_indices(&progress, 4, 10, 3).is_none());
    }

    #[test]
    fn integrity_check_keeps_durable_progress_visible_while_recounting() {
        assert_eq!(initial_bt_progress(true, 1024), (0, 1024));
        assert_eq!(initial_bt_progress(false, 1024), (1024, 1024));
    }
}
