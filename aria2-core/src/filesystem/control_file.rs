use crate::error::{Aria2Error, Result};
use std::path::{Path, PathBuf};

const CONTROL_MAGIC: &[u8; 4] = b"A2CF";
const CONTROL_VERSION: u16 = 1;
const FLAG_HAS_CHECKSUM: u8 = 0x01;
const FLAG_TORRENT_CHECKPOINT: u8 = 0x02;
const FLAG_TORRENT_INFO_HASH: u8 = 0x04;
const CONTROL_HEADER_LEN: usize = 39;

#[derive(Debug, Clone)]
pub struct ControlFile {
    path: PathBuf,
    total_length: u64,
    completed_length: u64,
    upload_length: u64,
    bitfield: Vec<u8>,
    num_pieces: usize,
    checksum_algo: u8,
    checksum_value: Vec<u8>,
    torrent_checkpoint: bool,
    torrent_info_hash: Option<[u8; 20]>,
}

impl ControlFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn total_length(&self) -> u64 {
        self.total_length
    }
    pub fn completed_length(&self) -> u64 {
        self.completed_length
    }
    pub fn bitfield(&self) -> &[u8] {
        &self.bitfield
    }
    pub fn set_checksum(&mut self, algo: u8, value: Vec<u8>) {
        self.checksum_algo = algo;
        self.checksum_value = value;
    }
    pub fn checksum_algo(&self) -> u8 {
        self.checksum_algo
    }

    /// Mark this Rust-owned checkpoint as BitTorrent state.
    ///
    /// The marker prevents a generic HTTP/FTP checkpoint with the same output
    /// path and shape from being mistaken for verified torrent pieces.
    pub fn mark_torrent_checkpoint(&mut self) {
        self.torrent_checkpoint = true;
    }

    pub fn is_torrent_checkpoint(&self) -> bool {
        self.torrent_checkpoint
    }

    /// Store the torrent identity for a Rust-owned BitTorrent checkpoint.
    pub fn set_torrent_info_hash(&mut self, info_hash: [u8; 20]) {
        self.torrent_info_hash = Some(info_hash);
    }

    pub fn torrent_info_hash(&self) -> Option<[u8; 20]> {
        self.torrent_info_hash
    }

    /// Replace the persisted piece bitfield with a complete snapshot.
    pub fn set_bitfield(&mut self, bitfield: Vec<u8>) {
        self.bitfield = bitfield;
    }

    pub async fn open_or_create(
        ctrl_path: &Path,
        total_length: u64,
        num_pieces: usize,
    ) -> Result<Self> {
        if ctrl_path.exists() {
            Self::load(ctrl_path).await?.ok_or_else(|| {
                Aria2Error::FileIo(format!(
                    "Failed to load control file: {}",
                    ctrl_path.display()
                ))
            })
        } else {
            let bitfield_len = num_pieces.div_ceil(8);
            Ok(Self {
                path: ctrl_path.to_path_buf(),
                total_length,
                completed_length: 0,
                upload_length: 0,
                bitfield: vec![0u8; bitfield_len],
                num_pieces,
                checksum_algo: 0,
                checksum_value: Vec::new(),
                torrent_checkpoint: false,
                torrent_info_hash: None,
            })
        }
    }

    pub async fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }

        let data = tokio::fs::read(path)
            .await
            .map_err(|e| Aria2Error::FileIo(format!("{}: {e}", path.display())))?;

        if data.len() < CONTROL_HEADER_LEN {
            return Err(Aria2Error::FileIo(format!(
                "Truncated control file header: {} bytes (expected at least {})",
                data.len(),
                CONTROL_HEADER_LEN
            )));
        }

        if &data[0..4] != CONTROL_MAGIC {
            return Err(Aria2Error::FileIo("Invalid control file magic".to_string()));
        }

        let version = u16_from_le(&data[4..6]);
        if version > CONTROL_VERSION {
            return Err(Aria2Error::FileIo(format!(
                "Unsupported version: {}",
                version
            )));
        }

        let flags = data[6];
        let total_length = u64_from_le(&data[7..15]);
        let completed_length = u64_from_le(&data[15..23]);
        let upload_length = u64_from_le(&data[23..31]);
        let bitfield_length = usize::try_from(u64_from_le(&data[31..39])).map_err(|_| {
            Aria2Error::FileIo("Control file bitfield length exceeds platform limits".to_string())
        })?;

        let mut offset = CONTROL_HEADER_LEN;
        let checksum_algo = if flags & FLAG_HAS_CHECKSUM != 0 {
            let algo = *data.get(offset).ok_or_else(|| {
                Aria2Error::FileIo("Truncated control file checksum algorithm".to_string())
            })?;
            offset += 1;
            algo
        } else {
            0
        };

        let checksum_value = if flags & FLAG_HAS_CHECKSUM != 0 && checksum_algo > 0 {
            let len = match checksum_algo {
                1 => 16,
                2 => 20,
                3 => 32,
                4 => 8,
                _ => {
                    return Err(Aria2Error::FileIo(format!(
                        "Unsupported control file checksum algorithm: {}",
                        checksum_algo
                    )));
                }
            };
            let end = offset.checked_add(len).ok_or_else(|| {
                Aria2Error::FileIo("Control file checksum length overflow".to_string())
            })?;
            if end > data.len() {
                return Err(Aria2Error::FileIo(
                    "Truncated control file checksum".to_string(),
                ));
            }
            let val = data[offset..end].to_vec();
            offset = end;
            val
        } else {
            Vec::new()
        };

        let torrent_info_hash = if flags & FLAG_TORRENT_INFO_HASH != 0 {
            let end = offset.checked_add(20).ok_or_else(|| {
                Aria2Error::FileIo("Control file torrent info hash length overflow".to_string())
            })?;
            if end > data.len() {
                return Err(Aria2Error::FileIo(
                    "Truncated control file torrent info hash".to_string(),
                ));
            }
            let mut info_hash = [0u8; 20];
            info_hash.copy_from_slice(&data[offset..end]);
            offset = end;
            Some(info_hash)
        } else {
            None
        };

        let bitfield_end = offset.checked_add(bitfield_length).ok_or_else(|| {
            Aria2Error::FileIo("Control file bitfield length overflow".to_string())
        })?;
        if bitfield_end != data.len() {
            return Err(Aria2Error::FileIo(format!(
                "Control file bitfield length mismatch: declared {}, available {}",
                bitfield_length,
                data.len().saturating_sub(offset)
            )));
        }
        let bitfield = data[offset..bitfield_end].to_vec();
        let num_pieces = bitfield.len() * 8;
        if completed_length > total_length {
            return Err(Aria2Error::FileIo(
                "Control file completed length exceeds total length".to_string(),
            ));
        }

        Ok(Some(Self {
            path: path.to_path_buf(),
            total_length,
            completed_length,
            upload_length,
            bitfield,
            num_pieces,
            checksum_algo,
            checksum_value,
            torrent_checkpoint: flags & FLAG_TORRENT_CHECKPOINT != 0,
            torrent_info_hash,
        }))
    }

    pub async fn save(&self) -> Result<()> {
        let mut buf = Vec::with_capacity(64 + self.bitfield.len());

        buf.extend_from_slice(CONTROL_MAGIC);
        buf.extend_from_slice(&CONTROL_VERSION.to_le_bytes());
        let mut flags: u8 = 0;
        if self.checksum_algo > 0 && !self.checksum_value.is_empty() {
            flags |= FLAG_HAS_CHECKSUM;
        }
        if self.torrent_checkpoint {
            flags |= FLAG_TORRENT_CHECKPOINT;
        }
        if self.torrent_info_hash.is_some() {
            flags |= FLAG_TORRENT_INFO_HASH;
        }
        buf.push(flags);
        buf.extend_from_slice(&self.total_length.to_le_bytes());
        buf.extend_from_slice(&self.completed_length.to_le_bytes());
        buf.extend_from_slice(&self.upload_length.to_le_bytes());
        buf.extend_from_slice(&(self.bitfield.len() as u64).to_le_bytes());

        if flags & FLAG_HAS_CHECKSUM != 0 {
            buf.push(self.checksum_algo);
            buf.extend_from_slice(&self.checksum_value);
        }

        if let Some(info_hash) = self.torrent_info_hash {
            buf.extend_from_slice(&info_hash);
        }

        buf.extend_from_slice(&self.bitfield);

        let tmp_path = self.path.with_extension("aria2.tmp");
        {
            tokio::fs::write(&tmp_path, &buf)
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
            if let Ok(f) = tokio::fs::File::open(&tmp_path).await {
                let _ = f.sync_all().await;
            }
        }
        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        Ok(())
    }

    pub fn mark_piece_done(&mut self, index: usize) {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.bitfield.len() {
            self.bitfield[byte_index] |= 1 << (7 - bit_index);
            self.completed_length = self.calculate_completed();
        }
    }

    pub fn is_piece_done(&self, index: usize) -> bool {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.bitfield.len() {
            (self.bitfield[byte_index] & (1 << (7 - bit_index))) != 0
        } else {
            false
        }
    }

    pub fn completed_pieces(&self) -> usize {
        self.bitfield.iter().map(|b| b.count_ones() as usize).sum()
    }

    fn calculate_completed(&self) -> u64 {
        let bits = self.completed_pieces() as u64;
        if self.total_length == 0 || self.num_pieces == 0 {
            return 0;
        }
        let piece_size = self.total_length / self.num_pieces as u64;
        bits * piece_size
    }

    pub fn update_completed_length(&mut self, length: u64) {
        self.completed_length = length.min(self.total_length);
    }

    pub fn control_path_for(output_path: &Path) -> PathBuf {
        let mut p = output_path.to_path_buf();
        p.set_extension(".aria2");
        p
    }
}

fn u16_from_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u64_from_le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_control_file_new_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.aria2");

        let cf = ControlFile::open_or_create(&path, 10000, 10).await.unwrap();
        assert_eq!(cf.total_length(), 10000);
        assert_eq!(cf.completed_length(), 0);
        assert!(!cf.is_piece_done(0));

        cf.save().await.unwrap();

        assert!(path.exists());
        let data = tokio::fs::read(&path).await.unwrap();
        assert_eq!(&data[0..4], b"A2CF");
    }

    #[tokio::test]
    async fn test_control_file_mark_and_check_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.aria2");

        let mut cf = ControlFile::open_or_create(&path, 1000, 8).await.unwrap();

        cf.mark_piece_done(0);
        cf.mark_piece_done(3);
        cf.mark_piece_done(7);

        assert!(cf.is_piece_done(0));
        assert!(!cf.is_piece_done(1));
        assert!(cf.is_piece_done(3));
        assert!(!cf.is_piece_done(5));
        assert!(cf.is_piece_done(7));
        assert_eq!(cf.completed_pieces(), 3);

        cf.save().await.unwrap();

        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.completed_pieces(), 3);
        assert!(loaded.is_piece_done(0));
        assert!(loaded.is_piece_done(7));
    }

    #[tokio::test]
    async fn test_control_file_roundtrip_with_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_hash.aria2");

        let mut cf = ControlFile::open_or_create(&path, 5000, 5).await.unwrap();
        cf.checksum_algo = 2;
        cf.checksum_value = vec![0xAB; 20];
        cf.mark_piece_done(0);
        cf.mark_piece_done(2);
        cf.save().await.unwrap();

        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.total_length(), 5000);
        assert_eq!(loaded.checksum_algo, 2);
        assert_eq!(loaded.completed_pieces(), 2);
    }

    #[tokio::test]
    async fn test_control_file_atomic_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_atomic.aria2");

        let mut cf = ControlFile::open_or_create(&path, 999, 4).await.unwrap();
        cf.mark_piece_done(1);
        cf.save().await.unwrap();

        let tmp_path = path.with_extension("aria2.tmp");
        assert!(!tmp_path.exists());
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_control_file_load_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.aria2");
        let result = ControlFile::load(&path).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_control_file_load_invalid_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.aria2");
        tokio::fs::write(&path, b"NOT_A2CF_DATA").await.unwrap();

        let result = ControlFile::load(&path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_control_file_load_truncated_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.aria2");
        for length in [0usize, 7, 8, 38] {
            let mut data = vec![0u8; length];
            if length >= 4 {
                data[..4].copy_from_slice(CONTROL_MAGIC);
            }
            tokio::fs::write(&path, &data).await.unwrap();
            assert!(ControlFile::load(&path).await.is_err(), "length={length}");
        }
    }

    #[tokio::test]
    async fn test_control_file_rejects_invalid_checksum_and_bitfield_lengths() {
        let dir = tempfile::tempdir().expect("failed to create temporary directory");
        let path = dir.path().join("malformed.aria2");
        let mut data = vec![0u8; CONTROL_HEADER_LEN];
        data[..4].copy_from_slice(CONTROL_MAGIC);
        data[6] = FLAG_HAS_CHECKSUM;
        data[31..39].copy_from_slice(&0u64.to_le_bytes());
        data.push(2);
        tokio::fs::write(&path, &data).await.unwrap();
        assert!(ControlFile::load(&path).await.is_err());

        let mut data = vec![0u8; CONTROL_HEADER_LEN];
        data[..4].copy_from_slice(CONTROL_MAGIC);
        data[31..39].copy_from_slice(&1u64.to_le_bytes());
        tokio::fs::write(&path, &data).await.unwrap();
        assert!(ControlFile::load(&path).await.is_err());
    }

    #[tokio::test]
    async fn test_control_path_for_output() {
        let out = Path::new("/downloads/file.iso");
        let ctrl = ControlFile::control_path_for(out);
        assert_eq!(ctrl.extension().unwrap().to_str().unwrap(), "aria2");
        assert!(ctrl.to_str().unwrap().ends_with(".aria2"));
    }

    #[tokio::test]
    async fn test_control_file_update_completed_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_len.aria2");

        let mut cf = ControlFile::open_or_create(&path, 8000, 8).await.unwrap();
        cf.update_completed_length(3500);
        assert_eq!(cf.completed_length(), 3500);

        cf.update_completed_length(9000);
        assert_eq!(cf.completed_length(), 8000);
    }
}
