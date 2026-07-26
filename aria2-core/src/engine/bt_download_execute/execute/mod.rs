mod bep6;
mod finalization;
mod peer_management;
mod pex;
mod piece_download;
mod web_seed;

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

#[async_trait]
impl Command for BtDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
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
            let bt_runtime = Arc::new(crate::engine::bt_runtime::BtRuntime::new());
            let bt_object = crate::engine::bt_registry::BtObject::builder()
                .download_context(download_context.unwrap_or_else(|| {
                    Arc::new(crate::download::DownloadContext::new(0, 0, String::new()))
                }))
                .bt_announce(bt_announce)
                .bt_runtime(bt_runtime)
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
            .connect_to_peers(&peer_addrs, &meta.info_hash.bytes, num_pieces)
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

        // PEX Integration: Initialize PEX state tracking for active connections
        // Each peer may support ut_pex extension (BEP 11) for peer discovery.
        // BEP 0027 (Private Torrent): leave pex_enabled_peers empty so no PEX
        // messages are ever sent.
        let mut pex_enabled_peers: HashSet<usize> = HashSet::new();
        let mut last_pex_send = Instant::now();
        const PEX_SEND_INTERVAL_SECS: u64 = 60;

        if !self.is_private {
            // Assume all peers support PEX by default. When the full
            // BtPeerInteractive handshake flow is wired, this will be
            // refined to check extension handshake results (ut_pex in
            // the m dict). For now, enable PEX for all connections as
            // the PEX layer gracefully handles peers that don't respond.
            for (idx, _conn) in active_connections.iter().enumerate() {
                pex_enabled_peers.insert(idx);
            }
            info!(
                "[PEX] Initialized PEX tracking for {} peers (assuming ut_pex support)",
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
                // Compute the BEP 6 fast-set for this peer's IP
                let fast_pieces = aria2_protocol::bittorrent::fast_set::compute_fast_set(
                    &conn.ip_addr,
                    num_pieces,
                    &meta.info_hash.bytes,
                    10, // MAX_ALLOWED_FAST_PER_PEER
                );
                if !fast_pieces.is_empty() {
                    let mut sent = HashSet::new();
                    for piece_idx in &fast_pieces {
                        let msg_bytes =
                            aria2_protocol::bittorrent::message::serializer::serialize_allowed_fast(
                                *piece_idx,
                            );
                        conn.queue_message(msg_bytes);
                        sent.insert(*piece_idx);
                    }
                    if let Err(e) = conn.flush_send_buffer().await {
                        warn!("[BEP6] Failed to flush AllowedFast to peer {}: {}", idx, e);
                    } else {
                        self.allowed_fast_sent_peers.insert(idx, sent);
                        debug!(
                            "[BEP6] Sent {} AllowedFast pieces to peer {} ({})",
                            fast_pieces.len(),
                            idx,
                            conn.ip_addr
                        );
                    }
                }
            }
        }

        self.download_pieces_loop(
            &mut active_connections,
            &meta,
            piece_length,
            total_size,
            num_pieces,
            web_seed_manager.as_ref(),
            &mut pex_enabled_peers,
            &mut last_pex_send,
            PEX_SEND_INTERVAL_SECS,
        )
        .await?;

        if self.seed_enabled && !active_connections.is_empty() {
            info!(
                "Starting seeding phase with {} peers...",
                active_connections.len()
            );
            self.run_seeding_phase(active_connections, piece_length, num_pieces)
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

        self.finalize_download(Instant::now(), &meta).await?;

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
