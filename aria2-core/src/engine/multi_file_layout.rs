use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFileEntry {
    pub index: usize,
    pub path: String,
    pub length: u64,
    pub completed_length: u64,
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: Vec<String>,
    pub length: u64,
    pub start_piece: u32,
    pub end_piece: u32,
    pub start_offset_in_piece: u32,
    pub end_offset_in_piece: u32,
    pub absolute_path: PathBuf,
}

#[derive(Clone)]
pub struct MultiFileLayout {
    base_dir: PathBuf,
    files: Vec<FileInfo>,
    piece_length: u32,
    total_pieces: u32,
    total_size: u64,
    is_single_file: bool,
}

impl MultiFileLayout {
    pub fn from_info_dict(info: &InfoDict, base_dir: &Path) -> Result<Self, String> {
        let piece_length = info.piece_length;
        let total_pieces = info.pieces.len() as u32;

        if let Some(length) = info.length {
            let name = &info.name;
            let absolute_path = base_dir.join(name);

            let total_size = length;

            let start_piece = 0u32;
            let end_piece = if total_size == 0 {
                0
            } else if total_pieces > 0 {
                total_pieces - 1
            } else {
                0
            };
            let start_offset_in_piece = 0u32;
            let end_offset_in_piece = if total_size == 0 {
                0
            } else {
                ((total_size - 1) % piece_length as u64 + 1) as u32
            };

            let file_info = FileInfo {
                path: vec![name.clone()],
                length,
                start_piece,
                end_piece,
                start_offset_in_piece,
                end_offset_in_piece,
                absolute_path,
            };

            info!(
                "Single-file layout: name={}, length={}, pieces={}",
                name, length, total_pieces
            );

            Ok(Self {
                base_dir: base_dir.to_path_buf(),
                files: vec![file_info],
                piece_length,
                total_pieces,
                total_size,
                is_single_file: true,
            })
        } else if let Some(ref files) = info.files {
            if files.is_empty() {
                return Err("files list is empty".to_string());
            }

            let mut file_infos = Vec::with_capacity(files.len());
            let mut running_offset: u64 = 0;
            let mut computed_total_size: u64 = 0;

            for (i, entry) in files.iter().enumerate() {
                let start_byte = running_offset;
                let end_byte = start_byte + entry.length;

                let pl = piece_length as u64;

                let start_piece = (start_byte / pl) as u32;
                let start_offset_in_piece = (start_byte % pl) as u32;

                let end_piece = if entry.length == 0 && start_byte == 0 {
                    0
                } else if end_byte == 0 {
                    0
                } else {
                    ((end_byte - 1) / pl) as u32
                };
                let end_offset_in_piece = if entry.length == 0 {
                    0
                } else {
                    ((end_byte - 1) % pl + 1) as u32
                };

                let abs_path = base_dir.join(entry.path.join("\\"));
                debug!(
                    "File[{}]: path={:?}, bytes=[{}..{}), pieces=[{}..{}] offsets=[{}..{})",
                    i,
                    entry.path,
                    start_byte,
                    end_byte,
                    start_piece,
                    end_piece,
                    start_offset_in_piece,
                    end_offset_in_piece
                );

                file_infos.push(FileInfo {
                    path: entry.path.clone(),
                    length: entry.length,
                    start_piece,
                    end_piece,
                    start_offset_in_piece,
                    end_offset_in_piece,
                    absolute_path: abs_path,
                });

                running_offset = end_byte;
                computed_total_size += entry.length;
            }

            info!(
                "Multi-file layout: {} files, total_size={}, pieces={}",
                file_infos.len(),
                computed_total_size,
                total_pieces
            );

            Ok(Self {
                base_dir: base_dir.to_path_buf(),
                files: file_infos,
                piece_length,
                total_pieces,
                total_size: computed_total_size,
                is_single_file: false,
            })
        } else {
            Err("InfoDict has neither length nor files field".to_string())
        }
    }

    pub fn create_directories(&self) -> Result<(), String> {
        for (i, file) in self.files.iter().enumerate() {
            if let Some(parent) = file.absolute_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "Failed to create directory {:?} for file[{}] {:?}: {}",
                        parent, i, file.path, e
                    )
                })?;
                debug!("Created directory: {:?}", parent);
            }
        }
        Ok(())
    }

    pub fn resolve_file_offset(&self, piece_idx: u32, offset_in_piece: u32) -> Option<(usize, u64)> {
        let global_byte = piece_idx as u64 * self.piece_length as u64 + offset_in_piece as u64;

        if global_byte >= self.total_size {
            return None;
        }

        for (i, file) in self.files.iter().enumerate() {
            let file_start = file.start_piece as u64 * self.piece_length as u64
                + file.start_offset_in_piece as u64;
            let file_end = file_start + file.length;

            if global_byte >= file_start && global_byte < file_end {
                return Some((i, global_byte - file_start));
            }
        }

        None
    }

    pub fn file_absolute_path(&self, file_index: usize) -> Option<&Path> {
        self.files.get(file_index).map(|f| f.absolute_path.as_path())
    }

    pub fn file_completed_bytes(&self, file_idx: usize, bitfield: &[u8]) -> u64 {
        let file = match self.files.get(file_idx) {
            Some(f) => f,
            None => return 0,
        };

        if file.length == 0 {
            return 0;
        }

        let pl = self.piece_length as u64;
        let mut completed: u64 = 0;

        for piece_idx in file.start_piece..=file.end_piece {
            let byte_index = piece_idx as usize / 8;
            let bit_index = 7 - (piece_idx as usize % 8);

            if byte_index >= bitfield.len() {
                break;
            }

            let is_complete = (bitfield[byte_index] >> bit_index) & 1 == 1;

            if !is_complete {
                continue;
            }

            if piece_idx == file.start_piece && piece_idx == file.end_piece {
                completed += file.length;
            } else if piece_idx == file.start_piece {
                let bytes_in_this_piece = pl - file.start_offset_in_piece as u64;
                completed += bytes_in_this_piece.min(file.length);
            } else if piece_idx == file.end_piece {
                completed += file.end_offset_in_piece as u64;
            } else {
                completed += pl;
            }
        }

        completed.min(file.length)
    }

    pub fn file_list(&self) -> Vec<TorrentFileEntry> {
        self.files
            .iter()
            .enumerate()
            .map(|(i, f)| TorrentFileEntry {
                index: i,
                path: f.path.join("/"),
                length: f.length,
                completed_length: 0,
            })
            .collect()
    }

    pub fn is_multi_file(&self) -> bool {
        !self.is_single_file
    }

    pub fn num_files(&self) -> usize {
        self.files.len()
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    pub fn total_pieces(&self) -> u32 {
        self.total_pieces
    }

    pub fn get_file_info(&self, index: usize) -> Option<&FileInfo> {
        self.files.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_single_file_info_dict() -> InfoDict {
        InfoDict {
            name: "single_file.bin".to_string(),
            piece_length: 512,
            pieces: vec![[0u8; 20], [1u8; 20]],
            length: Some(1024),
            files: None,
            private: None,
        }
    }

    fn make_multi_file_info_dict() -> InfoDict {
        InfoDict {
            name: "multi_dir".to_string(),
            piece_length: 512,
            pieces: vec![[0u8; 20], [1u8; 20], [2u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry {
                    length: 500,
                    path: vec!["dir1".to_string(), "file1.txt".to_string()],
                },
                FileEntry {
                    length: 524,
                    path: vec!["dir2".to_string(), "file2.dat".to_string()],
                },
                FileEntry {
                    length: 300,
                    path: vec!["dir3".to_string(), "file3.log".to_string()],
                },
            ]),
            private: None,
        }
    }

    #[test]
    fn test_from_info_dict_single_file() {
        let info = make_single_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        assert_eq!(layout.num_files(), 1);
        assert!(!layout.is_multi_file());
        assert_eq!(layout.total_size(), 1024);
        assert_eq!(layout.piece_length(), 512);
        assert_eq!(layout.total_pieces(), 2);

        let file = layout.get_file_info(0).unwrap();
        assert_eq!(file.path, vec!["single_file.bin"]);
        assert_eq!(file.length, 1024);
        assert_eq!(file.start_piece, 0);
        assert_eq!(file.end_piece, 1);
        assert_eq!(file.start_offset_in_piece, 0);
        assert_eq!(file.end_offset_in_piece, 512);
    }

    #[test]
    fn test_from_info_dict_multi_file() {
        let info = make_multi_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        assert_eq!(layout.num_files(), 3);
        assert!(layout.is_multi_file());
        assert_eq!(layout.total_size(), 1324);

        let f0 = layout.get_file_info(0).unwrap();
        assert_eq!(f0.length, 500);
        assert_eq!(f0.start_piece, 0);
        assert_eq!(f0.end_piece, 0);
        assert_eq!(f0.start_offset_in_piece, 0);
        assert_eq!(f0.end_offset_in_piece, 500);

        let f1 = layout.get_file_info(1).unwrap();
        assert_eq!(f1.length, 524);
        assert_eq!(f1.start_piece, 0);
        assert_eq!(f1.end_piece, 1);
        assert_eq!(f1.start_offset_in_piece, 500);
        assert_eq!(f1.end_offset_in_piece, 512);

        let f2 = layout.get_file_info(2).unwrap();
        assert_eq!(f2.length, 300);
        assert_eq!(f2.start_piece, 2);
        assert_eq!(f2.end_piece, 2);
        assert_eq!(f2.start_offset_in_piece, 0);
        assert_eq!(f2.end_offset_in_piece, 300);
    }

    #[test]
    fn test_from_info_dict_empty_files() {
        let info = InfoDict {
            name: "empty".to_string(),
            piece_length: 512,
            pieces: vec![],
            length: None,
            files: Some(vec![]),
            private: None,
        };
        let base = Path::new("/tmp/download");
        let result = MultiFileLayout::from_info_dict(&info, base);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_directories() {
        let info = make_multi_file_info_dict();
        let base = Path::new("d:/Code/aria2_rust/test_dirs");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let result = layout.create_directories();
        assert!(result.is_ok());

        let dir1 = base.join("dir1");
        let dir2 = base.join("dir2");
        let dir3 = base.join("dir3");

        assert!(dir1.exists());
        assert!(dir2.exists());
        assert!(dir3.exists());

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn test_resolve_file_offset_single_file() {
        let info = make_single_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let result = layout.resolve_file_offset(0, 0);
        assert_eq!(result, Some((0, 0)));

        let result = layout.resolve_file_offset(0, 256);
        assert_eq!(result, Some((0, 256)));

        let result = layout.resolve_file_offset(1, 0);
        assert_eq!(result, Some((0, 512)));

        let result = layout.resolve_file_offset(1, 511);
        assert_eq!(result, Some((0, 1023)));
    }

    #[test]
    fn test_resolve_file_offset_multi_file() {
        let info = make_multi_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let result = layout.resolve_file_offset(0, 0);
        assert_eq!(result, Some((0, 0)));

        let result = layout.resolve_file_offset(0, 499);
        assert_eq!(result, Some((0, 499)));

        let result = layout.resolve_file_offset(0, 500);
        assert_eq!(result, Some((1, 0)));

        let result = layout.resolve_file_offset(1, 0);
        assert_eq!(result, Some((1, 12)));

        let result = layout.resolve_file_offset(1, 200);
        assert_eq!(result, Some((1, 212)));

        let result = layout.resolve_file_offset(2, 50);
        assert_eq!(result, Some((2, 50)));
    }

    #[test]
    fn test_resolve_file_offset_boundary() {
        let info = make_multi_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let result = layout.resolve_file_offset(0, 499);
        assert_eq!(result, Some((0, 499)));

        let result = layout.resolve_file_offset(0, 500);
        assert_eq!(result, Some((1, 0)));

        let result = layout.resolve_file_offset(1, 511);
        assert_eq!(result, Some((1, 523)));

        let result = layout.resolve_file_offset(2, 0);
        assert_eq!(result, Some((2, 0)));
    }

    #[test]
    fn test_resolve_file_offset_out_of_range() {
        let info = make_single_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let result = layout.resolve_file_offset(1, 512);
        assert_eq!(result, None);

        let result = layout.resolve_file_offset(2, 0);
        assert_eq!(result, None);

        let result = layout.resolve_file_offset(u32::MAX, 0);
        assert_eq!(result, None);
    }

    #[test]
    fn test_file_completed_bytes() {
        let info = make_single_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let no_pieces = [0u8; 1];
        assert_eq!(layout.file_completed_bytes(0, &no_pieces), 0);

        let piece0_only = [0b10000000u8];
        assert_eq!(layout.file_completed_bytes(0, &piece0_only), 512);

        let both_pieces = [0b11000000u8];
        assert_eq!(layout.file_completed_bytes(0, &both_pieces), 1024);
    }

    #[test]
    fn test_file_list_returns_correct_entries() {
        let info = make_multi_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let list = layout.file_list();
        assert_eq!(list.len(), 3);

        assert_eq!(list[0].index, 0);
        assert_eq!(list[0].path, "dir1/file1.txt");
        assert_eq!(list[0].length, 500);
        assert_eq!(list[0].completed_length, 0);

        assert_eq!(list[1].index, 1);
        assert_eq!(list[1].path, "dir2/file2.dat");
        assert_eq!(list[1].length, 524);

        assert_eq!(list[2].index, 2);
        assert_eq!(list[2].path, "dir3/file3.log");
        assert_eq!(list[2].length, 300);
    }

    #[test]
    fn test_is_multi_file_flags() {
        let single = make_single_file_info_dict();
        let single_layout = MultiFileLayout::from_info_dict(&single, Path::new("/tmp")).unwrap();
        assert!(!single_layout.is_multi_file());

        let multi = make_multi_file_info_dict();
        let multi_layout = MultiFileLayout::from_info_dict(&multi, Path::new("/tmp")).unwrap();
        assert!(multi_layout.is_multi_file());
    }

    #[test]
    fn test_total_size_matches() {
        let single = make_single_file_info_dict();
        let single_layout = MultiFileLayout::from_info_dict(&single, Path::new("/tmp")).unwrap();
        assert_eq!(single_layout.total_size(), 1024);

        let multi = make_multi_file_info_dict();
        let multi_layout = MultiFileLayout::from_info_dict(&multi, Path::new("/tmp")).unwrap();
        assert_eq!(multi_layout.total_size(), 1324);
        assert_eq!(multi_layout.total_size(), 500 + 524 + 300);
    }

    #[test]
    fn test_file_absolute_path() {
        let info = make_multi_file_info_dict();
        let base = Path::new("/base/dir");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let p0 = layout.file_absolute_path(0).unwrap();
        assert_eq!(p0, Path::new("/base/dir/dir1/file1.txt"));

        let p1 = layout.file_absolute_path(1).unwrap();
        assert_eq!(p1, Path::new("/base/dir/dir2/file2.dat"));

        let p2 = layout.file_absolute_path(2).unwrap();
        assert_eq!(p2, Path::new("/base/dir/dir3/file3.log"));

        assert!(layout.file_absolute_path(3).is_none());
    }

    #[test]
    fn test_file_completed_bytes_multi_file_partial() {
        let info = make_multi_file_info_dict();
        let base = Path::new("/tmp/download");
        let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

        let only_piece0 = [0b10000000u8];
        assert_eq!(layout.file_completed_bytes(0, &only_piece0), 500);
        assert_eq!(layout.file_completed_bytes(1, &only_piece0), 12);
        assert_eq!(layout.file_completed_bytes(2, &only_piece0), 0);

        let piece0_and_1 = [0b11000000u8];
        assert_eq!(layout.file_completed_bytes(0, &piece0_and_1), 500);
        assert_eq!(layout.file_completed_bytes(1, &piece0_and_1), 524);
        assert_eq!(layout.file_completed_bytes(2, &piece0_and_1), 0);

        let all_pieces = [0b11100000u8];
        assert_eq!(layout.file_completed_bytes(0, &all_pieces), 500);
        assert_eq!(layout.file_completed_bytes(1, &all_pieces), 524);
        assert_eq!(layout.file_completed_bytes(2, &all_pieces), 300);
    }
}
