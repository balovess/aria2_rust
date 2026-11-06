use std::path::PathBuf;

use tracing::info;

use crate::config::parse_integer_segments;
use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::error::{Aria2Error, FatalError, Result};
use crate::util::rwlock_ext::RwLockRecover;

pub(super) fn parse_listen_ports(value: &str) -> std::result::Result<Vec<u16>, String> {
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
            layout.create_directories_durable().await.map_err(|e| {
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
                .map_err(|e| Aria2Error::BittorrentParse(format!("Torrent parse error: {}", e)))?;

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

    /// Returns a check for a checkpoint bitfield: whether every file that
    /// holds a verified piece still exists, so the checkpoint can be trusted.
    /// With `select-file` and no pre-allocation, files outside the selection
    /// (and selected files not reached yet) are never created, so requiring
    /// every file discarded valid progress on each restart. The closure owns
    /// its data because it is held across the checkpoint load.
    pub(super) fn bt_checkpoint_files_exist(
        &self,
    ) -> impl Fn(&[u8]) -> bool + Send + Sync + 'static {
        let layout = self.multi_file_layout.clone();
        let output_path = self.output_path.clone();
        move |bitfield: &[u8]| match layout.as_ref() {
            Some(layout) => (0..layout.num_files()).all(|index| {
                layout.file_completed_bytes(index, bitfield) == 0
                    || layout
                        .file_absolute_path(index)
                        .is_some_and(|path| path.is_file())
            }),
            None => output_path.is_file(),
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

#[cfg(test)]
mod tests {
    use crate::engine::bittorrent::download::command::BtDownloadCommand;
    use crate::engine::bittorrent::download::command_tests::build_test_torrent;
    use crate::engine::bittorrent::torrent::file_layout::MultiFileLayout;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    #[test]
    fn checkpoint_files_check_requires_only_files_with_verified_pieces() {
        let dir = tempfile::tempdir().expect("temporary BT output directory");
        let mut command = BtDownloadCommand::new(
            GroupId::new(73_101),
            &build_test_torrent(),
            &DownloadOptions::default(),
            None,
        )
        .expect("test torrent should construct");
        // Three files, one piece each.
        let file = |name: &str| FileEntry {
            length: 8,
            path: vec![name.into()],
        };
        let info = InfoDict {
            name: "selected".into(),
            piece_length: 8,
            pieces: vec![[0; 20]; 3],
            length: None,
            files: Some(vec![
                file("first.bin"),
                file("second.bin"),
                file("third.bin"),
            ]),
            private: None,
            meta_version: None,
            v2_files: None,
            pieces_root: None,
        };
        let layout = MultiFileLayout::from_info_dict(&info, &dir.path().join("selected"))
            .expect("multi-file layout should build");
        layout
            .create_directories()
            .expect("payload directories should be created");
        let first = layout.file_absolute_path(0).unwrap().to_path_buf();
        let second = layout.file_absolute_path(1).unwrap().to_path_buf();
        command.multi_file_layout = Some(layout);
        let files_exist = command.bt_checkpoint_files_exist();

        // Nothing verified: nothing needs to exist.
        assert!(files_exist(&[0b0000_0000]));
        // The first piece verified, its file missing: the checkpoint can't be trusted.
        assert!(!files_exist(&[0b1000_0000]));
        std::fs::write(&first, [0u8; 8]).unwrap();
        // Its file exists; the other files (not selected, never written) don't matter.
        assert!(files_exist(&[0b1000_0000]));
        assert!(!files_exist(&[0b1100_0000]));
        std::fs::write(&second, [0u8; 8]).unwrap();
        assert!(files_exist(&[0b1100_0000]));
    }
}
