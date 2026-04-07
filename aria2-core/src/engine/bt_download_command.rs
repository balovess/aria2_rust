use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use tracing::{info, debug, warn};

use crate::error::{Aria2Error, Result, RecoverableError, FatalError};
use crate::engine::command::{Command, CommandStatus};
use crate::request::request_group::{RequestGroup, GroupId, DownloadOptions};
use crate::filesystem::disk_writer::{DiskWriter, DefaultDiskWriter};

pub struct BtDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    torrent_data: Vec<u8>,
}

impl BtDownloadCommand {
    pub fn new(
        gid: GroupId,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Torrent parse failed: {}", e))))?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = meta.info.name.clone();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        let group = RequestGroup::new(gid, vec![format!("bt://{}", meta.info_hash.as_hex())], options.clone());
        info!("BtDownloadCommand created: {} -> {} ({} bytes, {} pieces)", meta.info.name, path.display(), meta.total_size(), meta.num_pieces());

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            output_path: path,
            started: false,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
        })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }
}

#[async_trait]
impl Command for BtDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e))))?;
            }
        }

        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&self.torrent_data)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Torrent parse error: {}", e))))?;

        {
            let mut g = self.group.write().await;
            g.set_total_length(meta.total_size()).await;
        }

        let piece_length = meta.info.piece_length;
        let total_size = meta.total_size();
        let num_pieces = meta.num_pieces();

        let mut piece_manager = aria2_protocol::bittorrent::piece::manager::PieceManager::new(
            num_pieces as u32, piece_length, total_size, meta.info.pieces.clone(),
        );

        let mut piece_picker = aria2_protocol::bittorrent::piece::picker::PiecePicker::new(num_pieces as u32);
        piece_picker.set_strategy(aria2_protocol::bittorrent::piece::picker::PieceSelectionStrategy::Sequential);

        let my_peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();
        let info_hash_raw = meta.info_hash.bytes;

        let announce_url = format!("{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&event=started&compact=1",
            meta.announce,
            urlencode_infohash(&info_hash_raw),
            urlencode_infohash(&my_peer_id),
            total_size,
        );

        let resp = reqwest::get(&announce_url).await
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: format!("Tracker HTTP failed: {}", e) }))?;
        let body = resp.bytes().await
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: format!("Tracker body read failed: {}", e) }))?;

        let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: format!("Tracker parse failed: {}", e) }))?;

        debug!("Tracker response: {} peers", tracker_resp.peer_count());

        if tracker_resp.is_failure() {
            return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: tracker_resp.failure_reason.unwrap_or_default() }));
        }

        let peer_addrs: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> = tracker_resp.peers.iter()
            .map(|p| aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&p.ip, p.port))
            .collect();

        if peer_addrs.is_empty() {
            return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: "No peers from tracker".into() }));
        }

        let mut active_connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = Vec::new();

        for addr in &peer_addrs {
            match aria2_protocol::bittorrent::peer::connection::PeerConnection::connect(addr, &info_hash_raw).await {
                Ok(mut conn) => {
                    conn.send_unchoke().await.ok();
                    conn.send_interested().await.ok();

                    let bf_len = (num_pieces + 7) / 8;
                    let empty_bf = vec![0u8; bf_len];
                    conn.send_bitfield(empty_bf.clone()).await.ok();

                    tokio::time::sleep(Duration::from_millis(100)).await;

                    for _ in 0..50 {
                        match tokio::time::timeout(Duration::from_secs(5), conn.read_message()).await {
                            Ok(Ok(Some(msg))) => {
                                use aria2_protocol::bittorrent::message::types::BtMessage;
                                if matches!(msg, BtMessage::Unchoke) { break; }
                            }
                            _ => break,
                        }
                    }

                    active_connections.push(conn);
                }
                Err(e) => { debug!("Failed to connect peer {}: {}", addr.ip, e); continue; }
            }
        }

        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: "All peer connections failed".into() }));
        }

        let mut writer = DefaultDiskWriter::new(&self.output_path);
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        const BLOCK_SIZE: u32 = 16384;
        const MAX_RETRIES: u32 = 3;

        loop {
            if piece_picker.is_complete() { break; }

            let next_piece_idx = match piece_picker.pick_next() {
                Some(idx) => idx as usize,
                None => { tokio::time::sleep(Duration::from_millis(100)).await; continue; }
            };

            let actual_piece_len = if next_piece_idx == num_pieces - 1 && total_size % piece_length as u64 != 0 {
                (total_size % piece_length as u64) as u32
            } else {
                piece_length
            };

            let num_blocks = (actual_piece_len + BLOCK_SIZE - 1) / BLOCK_SIZE;
            let mut piece_ok = false;

            for _retry in 0..MAX_RETRIES {
                let mut piece_data = Vec::with_capacity(actual_piece_len as usize);
                let mut blocks_received = 0u32;

                for block_idx in 0..num_blocks {
                    let offset = block_idx * BLOCK_SIZE;
                    let len = if offset + BLOCK_SIZE > actual_piece_len { actual_piece_len - offset } else { BLOCK_SIZE };

                    let req = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                        index: next_piece_idx as u32,
                        begin: offset,
                        length: len,
                    };

                    let mut got_block = false;
                    for conn in &mut active_connections {
                        if conn.send_request(req.clone()).await.is_err() { continue; }

                        match tokio::time::timeout(Duration::from_secs(3), async {
                            for _ in 0..10000 {
                                match conn.read_message().await {
                                    Ok(Some(msg)) => {
                                        if let aria2_protocol::bittorrent::message::types::BtMessage::Piece { index, begin, ref data } = msg {
                                            if index == next_piece_idx as u32 && begin == offset {
                                                return Ok(data.clone());
                                            }
                                        }
                                    }
                                    Ok(None) | Err(_) => continue,
                                }
                            }
                            Err(())
                        }).await {
                            Ok(Ok(data)) => {
                                piece_data.extend_from_slice(&data);
                                blocks_received += 1;
                                self.completed_bytes += data.len() as u64;
                                got_block = true;
                                break;
                            }
                            _ => continue,
                        }
                    }

                    if !got_block { break; }
                }

                if blocks_received == num_blocks {
                    if piece_manager.verify_piece_hash(next_piece_idx as u32, &piece_data) {
                        piece_manager.mark_piece_complete(next_piece_idx as u32);
                        piece_picker.mark_completed(next_piece_idx as u32);
                        writer.write(&piece_data).await.ok();

                        for conn in &mut active_connections {
                            conn.send_have(next_piece_idx as u32).await.ok();
                        }
                        piece_ok = true;
                        break;
                    } else {
                        warn!("SHA1 mismatch on piece {}, retrying...", next_piece_idx);
                    }
                } else {
                    warn!("Incomplete piece {}, received {}/{}", next_piece_idx, blocks_received, num_blocks);
                }
            }

            if !piece_ok {
                return Err(Aria2Error::Fatal(FatalError::Config(format!("Piece {} download failed after {} retries", next_piece_idx, MAX_RETRIES))));
            }

            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        writer.finalize().await.ok();

        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 { (self.completed_bytes as f64 / elapsed) as u64 } else { 0 }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.complete().await?;
        }

        info!("BT download done: {} ({} bytes)", self.output_path.display(), self.completed_bytes);
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 { CommandStatus::Running } else { CommandStatus::Pending }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }
}

fn urlencode_infohash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("%{:02X}", b)).collect()
}
