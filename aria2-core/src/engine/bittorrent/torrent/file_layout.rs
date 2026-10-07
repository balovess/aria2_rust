use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use aria2_protocol::bittorrent::torrent::parser::InfoDict;

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
    #[allow(dead_code)] // Base directory for multi-file torrent layouts
    base_dir: PathBuf,
    files: Vec<FileInfo>,
    piece_length: u32,
    total_pieces: u32,
    total_size: u64,
    piece_space_size: u64,
    is_single_file: bool,
}

impl MultiFileLayout {
    pub fn from_info_dict(info: &InfoDict, base_dir: &Path) -> Result<Self, String> {
        info.validate_paths()?;
        let piece_length = info.piece_length;
        let total_pieces =
            if info.meta_version == Some(2) {
                let piece_length = info.piece_length as u64;
                let piece_space = info.v2_files.as_deref().unwrap_or_default().iter().fold(
                    0u64,
                    |offset, file| {
                        if file.length == 0 {
                            offset
                        } else {
                            offset.div_ceil(piece_length) * piece_length + file.length
                        }
                    },
                );
                piece_space.div_ceil(piece_length) as u32
            } else {
                info.pieces.len() as u32
            };

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
                piece_space_size: total_size,
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

                if entry
                    .path
                    .first()
                    .is_some_and(|component| component == ".pad")
                {
                    running_offset = end_byte;
                    continue;
                }

                let pl = piece_length as u64;

                let start_piece = (start_byte / pl) as u32;
                let start_offset_in_piece = (start_byte % pl) as u32;

                let end_piece = if entry.length == 0 && start_byte == 0 || end_byte == 0 {
                    0
                } else {
                    ((end_byte - 1) / pl) as u32
                };
                let end_offset_in_piece = if entry.length == 0 {
                    0
                } else {
                    ((end_byte - 1) % pl + 1) as u32
                };

                // Build path using proper path separators
                let mut path_buf = base_dir.to_path_buf();
                for component in &entry.path {
                    path_buf.push(component);
                }
                let abs_path = path_buf;
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
                piece_space_size: running_offset,
                is_single_file: false,
            })
        } else if let Some(ref files) = info.v2_files {
            if files.is_empty() {
                return Err("v2 file tree contains no files".to_string());
            }
            let mut file_infos = Vec::with_capacity(files.len());
            let mut running_offset = 0u64;
            let mut computed_total_size = 0u64;
            let pl = piece_length as u64;
            for (i, entry) in files.iter().enumerate() {
                if entry.length == 0 {
                    continue;
                }
                running_offset = running_offset.div_ceil(pl) * pl;
                let start_byte = running_offset;
                let end_byte = start_byte + entry.length;
                let mut path_buf = base_dir.to_path_buf();
                for component in &entry.path {
                    path_buf.push(component);
                }
                file_infos.push(FileInfo {
                    path: entry.path.clone(),
                    length: entry.length,
                    start_piece: (start_byte / pl) as u32,
                    end_piece: ((end_byte - 1) / pl) as u32,
                    start_offset_in_piece: 0,
                    end_offset_in_piece: ((end_byte - 1) % pl + 1) as u32,
                    absolute_path: path_buf,
                });
                running_offset = end_byte;
                computed_total_size += entry.length;
                debug!(file_index = i, start_byte, end_byte, "v2 aligned file");
            }
            Ok(Self {
                base_dir: base_dir.to_path_buf(),
                files: file_infos,
                piece_length,
                total_pieces: running_offset.div_ceil(pl) as u32,
                total_size: computed_total_size,
                piece_space_size: running_offset,
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

    pub fn resolve_file_offset(
        &self,
        piece_idx: u32,
        offset_in_piece: u32,
    ) -> Option<(usize, u64)> {
        let global_byte = piece_idx as u64 * self.piece_length as u64 + offset_in_piece as u64;

        if global_byte >= self.piece_space_size {
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
        self.files
            .get(file_index)
            .map(|f| f.absolute_path.as_path())
    }

    /// Override the output path for one torrent file.
    ///
    /// Metalink torrent groups may map a torrent-relative `originalName` to a
    /// different user-visible path. The byte offsets remain those from the
    /// torrent; only the destination path changes.
    pub fn set_file_absolute_path(
        &mut self,
        file_index: usize,
        path: PathBuf,
    ) -> Result<(), String> {
        let file = self
            .files
            .get_mut(file_index)
            .ok_or_else(|| format!("invalid file index {file_index}"))?;
        file.absolute_path = path.clone();
        let relative = path.strip_prefix(&self.base_dir).unwrap_or(&path);
        file.path = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        Ok(())
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

    /// Number of payload bytes represented by a logical v2 piece. Alignment
    /// gaps are addressable but are not downloaded or written.
    pub fn content_bytes_in_piece(&self, piece_idx: u32) -> u64 {
        let start = piece_idx as u64 * self.piece_length as u64;
        let end = start + self.piece_length as u64;
        self.files
            .iter()
            .map(|file| {
                let file_start = file.start_piece as u64 * self.piece_length as u64;
                let file_end = file_start + file.length;
                end.min(file_end).saturating_sub(start.max(file_start))
            })
            .sum()
    }

    /// Return the next logical byte in this piece that belongs to a file.
    /// `piece_length` means the remainder is alignment padding.
    pub fn next_content_offset(&self, piece_idx: u32, offset_in_piece: u32) -> u32 {
        let current = piece_idx as u64 * self.piece_length as u64 + offset_in_piece as u64;
        self.files
            .iter()
            .map(|file| file.start_piece as u64 * self.piece_length as u64)
            .filter(|&start| start > current)
            .map(|start| (start - piece_idx as u64 * self.piece_length as u64) as u32)
            .min()
            .unwrap_or(self.piece_length)
    }

    pub fn piece_space_size(&self) -> u64 {
        self.piece_space_size
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
#[path = "file_layout/tests.rs"]
mod tests;
