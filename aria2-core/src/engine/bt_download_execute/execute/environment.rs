use std::path::PathBuf;

use tracing::info;

use crate::config::parse_integer_segments;
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::error::{Aria2Error, FatalError, Result};
use crate::util::rwlock_ext::RwLockRecover;

pub(crate) fn parse_listen_ports(value: &str) -> std::result::Result<Vec<u16>, String> {
    let ports = parse_integer_segments(value, 1024, u16::MAX as i64)?
        .into_iter()
        .flat_map(|range| range.map(|port| port as u16))
        .collect::<Vec<_>>();
    Ok(ports)
}

impl BtDownloadCommand {
    /// Prepare the download environment: create output directories, parse torrent metadata, and set total length on the request group.
    pub(super) async fn prepare_environment(
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

    pub(super) fn integrity_files(
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

    pub(super) fn bt_payload_exists(&self) -> bool {
        match self.multi_file_layout.as_ref() {
            Some(layout) => layout.file_list().into_iter().all(|entry| {
                layout
                    .file_absolute_path(entry.index)
                    .is_some_and(|path| entry.length == 0 || path.is_file())
            }),
            None => self.output_path.is_file(),
        }
    }

    pub(super) async fn create_zero_length_payload(&self) -> Result<()> {
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
