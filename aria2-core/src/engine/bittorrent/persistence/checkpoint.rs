//! Rust-owned BitTorrent piece checkpoints.
//!
//! The public sidecar path remains the familiar `.aria2` location. New bytes
//! use aria2's native control-file layout plus the small project extension
//! needed for verified-piece metadata; the retired A2CF reader is migration
//! compatibility only.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::filesystem::control_file::{ControlFile, ControlFileInFlightPiece};

pub(crate) struct BtCheckpoint {
    control_file: Option<ControlFile>,
    path: PathBuf,
    total_length: u64,
    piece_length: u32,
    num_pieces: usize,
    info_hash: [u8; 20],
}

impl BtCheckpoint {
    pub(crate) async fn open(
        output_path: &Path,
        payload_exists: bool,
        total_length: u64,
        piece_length: u32,
        num_pieces: usize,
        info_hash: [u8; 20],
    ) -> Result<Self> {
        let path = ControlFile::control_path_for(output_path);
        let expected_bitfield_len = num_pieces.div_ceil(8);
        let control_file = match ControlFile::load(&path).await {
            Ok(Some(mut control_file))
                if control_file.is_torrent_checkpoint()
                    && control_file.total_length() == total_length
                    && payload_exists
                    && control_file.bitfield().len() == expected_bitfield_len
                    && valid_trailing_bits(control_file.bitfield(), num_pieces)
                    && control_file.torrent_info_hash() == Some(info_hash)
                    && control_file
                        .torrent_piece_length()
                        .is_none_or(|stored| stored == piece_length) =>
            {
                // Legacy checkpoints written before piece-length persistence
                // are still safe to restore because the torrent info hash
                // identifies the complete info dictionary. Normalize their
                // byte-only bitfield before the next native-format save.
                control_file.normalize_legacy_piece_layout(num_pieces)?;
                control_file.set_torrent_piece_length(piece_length);
                Some(control_file)
            }
            Ok(Some(_)) => {
                remove_stale(&path).await;
                None
            }
            Ok(None) => None,
            Err(error) => {
                tracing::debug!(
                    path = %path.display(),
                    %error,
                    "Ignoring unreadable BitTorrent checkpoint"
                );
                remove_stale(&path).await;
                None
            }
        };

        let mut checkpoint = Self {
            control_file,
            path,
            total_length,
            piece_length,
            num_pieces,
            info_hash,
        };
        if checkpoint.control_file.is_none() {
            checkpoint.control_file = Some(
                ControlFile::open_or_create(&checkpoint.path, total_length, num_pieces).await?,
            );
            if let Some(control_file) = checkpoint.control_file.as_mut() {
                control_file.mark_torrent_checkpoint();
                control_file.set_torrent_info_hash(info_hash);
                control_file.set_torrent_piece_length(piece_length);
            }
        }
        Ok(checkpoint)
    }

    pub(crate) fn bitfield(&self) -> Option<&[u8]> {
        self.control_file
            .as_ref()
            .map(ControlFile::bitfield)
            .filter(|bitfield| bitfield.len() == self.num_pieces.div_ceil(8))
            .filter(|bitfield| valid_trailing_bits(bitfield, self.num_pieces))
    }

    pub(crate) fn completed_length(&self) -> u64 {
        let Some(bitfield) = self.bitfield() else {
            return 0;
        };

        (0..self.num_pieces)
            .filter(|&index| bit_is_set(bitfield, index))
            .map(|index| {
                let offset = index as u64 * self.piece_length as u64;
                self.total_length
                    .saturating_sub(offset)
                    .min(self.piece_length as u64)
            })
            .sum()
    }

    pub(crate) fn in_flight_pieces(&self) -> &[ControlFileInFlightPiece] {
        self.control_file
            .as_ref()
            .map(ControlFile::in_flight_pieces)
            .unwrap_or_default()
    }

    pub(crate) async fn save(&mut self, bitfield: &[u8], completed_length: u64) -> Result<()> {
        self.save_with_in_flight_pieces(bitfield, completed_length, &[])
            .await
    }

    pub(crate) async fn save_with_in_flight_pieces(
        &mut self,
        bitfield: &[u8],
        completed_length: u64,
        in_flight_pieces: &[ControlFileInFlightPiece],
    ) -> Result<()> {
        if bitfield.len() != self.num_pieces.div_ceil(8) {
            return Err(crate::error::Aria2Error::FileIo(format!(
                "BitTorrent checkpoint bitfield length mismatch: expected {}, got {}",
                self.num_pieces.div_ceil(8),
                bitfield.len()
            )));
        }
        if !valid_trailing_bits(bitfield, self.num_pieces) {
            return Err(crate::error::Aria2Error::FileIo(
                "BitTorrent checkpoint contains set trailing bits".into(),
            ));
        }
        let control_file = self.control_file.as_mut().ok_or_else(|| {
            crate::error::Aria2Error::FileIo("BitTorrent checkpoint is unavailable".into())
        })?;
        control_file.mark_torrent_checkpoint();
        if control_file.torrent_info_hash() != Some(self.info_hash) {
            return Err(crate::error::Aria2Error::FileIo(
                "BitTorrent checkpoint torrent identity mismatch".into(),
            ));
        }
        if control_file.torrent_piece_length() != Some(self.piece_length) {
            return Err(crate::error::Aria2Error::FileIo(
                "BitTorrent checkpoint piece length mismatch".into(),
            ));
        }
        // Keep the in-memory snapshot unchanged until the durable replacement
        // succeeds. A failed disk write must not make this instance appear
        // newer than the checkpoint that can actually be restored.
        let mut next = control_file.clone();
        next.set_bitfield(bitfield.to_vec());
        next.set_in_flight_pieces(in_flight_pieces.to_vec());
        next.update_completed_length(completed_length.min(self.total_length));
        next.save().await?;
        *control_file = next;
        Ok(())
    }

    pub(crate) async fn save_verified_pieces<I>(
        &mut self,
        indices: I,
        completed_length: u64,
    ) -> Result<()>
    where
        I: IntoIterator<Item = usize>,
    {
        let mut bitfield = vec![0u8; self.num_pieces.div_ceil(8)];
        for index in indices {
            if index < self.num_pieces {
                bitfield[index / 8] |= 1 << (7 - index % 8);
            }
        }
        self.save(&bitfield, completed_length).await
    }

    pub(crate) async fn remove(self) -> Result<()> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(crate::error::Aria2Error::FileIo(format!(
                "Failed to remove BitTorrent checkpoint {}: {error}",
                self.path.display()
            ))),
        }
    }
}

fn bit_is_set(bitfield: &[u8], index: usize) -> bool {
    bitfield
        .get(index / 8)
        .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
}

fn valid_trailing_bits(bitfield: &[u8], num_pieces: usize) -> bool {
    if num_pieces == 0 {
        return bitfield.is_empty();
    }
    let unused_bits = (8 - num_pieces % 8) % 8;
    unused_bits == 0
        || bitfield
            .last()
            .is_some_and(|byte| byte & ((1u8 << unused_bits) - 1) == 0)
}

async fn remove_stale(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(path = %path.display(), %error, "Failed to remove stale BitTorrent checkpoint");
    }
}

#[cfg(test)]
mod tests {
    use super::BtCheckpoint;
    use crate::filesystem::control_file::{ControlFile, ControlFileInFlightPiece};

    #[tokio::test]
    async fn checkpoint_restores_piece_sized_completed_length() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 10]).unwrap();
        let info_hash = [0x11; 20];
        let mut checkpoint = BtCheckpoint::open(&output, true, 10, 4, 3, info_hash)
            .await
            .unwrap();

        checkpoint.save(&[0b1010_0000], 6).await.unwrap();
        assert_eq!(checkpoint.completed_length(), 6);

        let restored = BtCheckpoint::open(&output, true, 10, 4, 3, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0b1010_0000].as_slice()));
        assert_eq!(restored.completed_length(), 6);
        let control = ControlFile::load(&ControlFile::control_path_for(&output))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(control.torrent_piece_length(), Some(4));
    }

    #[tokio::test]
    async fn checkpoint_restores_only_durable_in_flight_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, vec![0xA5; 64 * 1024]).unwrap();
        let info_hash = [0x29; 20];
        let mut checkpoint = BtCheckpoint::open(&output, true, 64 * 1024, 64 * 1024, 1, info_hash)
            .await
            .unwrap();
        let partial = ControlFileInFlightPiece {
            index: 0,
            length: 64 * 1024,
            bitfield: vec![0b1010_0000],
        };

        checkpoint
            .save_with_in_flight_pieces(&[0], 0, &[partial.clone()])
            .await
            .unwrap();

        let restored = BtCheckpoint::open(&output, true, 64 * 1024, 64 * 1024, 1, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0].as_slice()));
        assert_eq!(restored.in_flight_pieces(), &[partial]);
    }

    #[tokio::test]
    async fn checkpoint_accepts_a_multi_file_output_directory() {
        let dir = tempfile::tempdir().unwrap();
        let info_hash = [0x77; 20];
        let mut checkpoint = BtCheckpoint::open(dir.path(), false, 8, 4, 2, info_hash)
            .await
            .expect("a multi-file output directory needs a sidecar checkpoint");

        checkpoint.save(&[0xC0], 8).await.unwrap();

        let sidecar = ControlFile::control_path_for(dir.path());
        assert!(sidecar.is_file());
        let restored = BtCheckpoint::open(dir.path(), true, 8, 4, 2, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.completed_length(), 8);
    }

    #[tokio::test]
    async fn checkpoint_discards_different_piece_length() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 10]).unwrap();
        let info_hash = [0x55; 20];
        let mut checkpoint = BtCheckpoint::open(&output, true, 10, 4, 3, info_hash)
            .await
            .unwrap();
        checkpoint.save(&[0b1100_0000], 8).await.unwrap();

        let restored = BtCheckpoint::open(&output, true, 10, 5, 3, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0u8].as_slice()));
        assert_eq!(restored.completed_length(), 0);
    }

    #[tokio::test]
    async fn checkpoint_accepts_legacy_sidecar_without_piece_length() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 8]).unwrap();
        let info_hash = [0x66; 20];
        let path = ControlFile::control_path_for(&output);

        // Retired A2CF bytes: torrent marker + info hash, but no persisted
        // piece length. The next save must rewrite this as native v1.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(b"A2CF");
        legacy.extend_from_slice(&1u16.to_le_bytes());
        legacy.push(0x02 | 0x04);
        legacy.extend_from_slice(&8u64.to_le_bytes());
        legacy.extend_from_slice(&8u64.to_le_bytes());
        legacy.extend_from_slice(&0u64.to_le_bytes());
        legacy.extend_from_slice(&1u64.to_le_bytes());
        legacy.extend_from_slice(&info_hash);
        legacy.push(0xC0);
        tokio::fs::write(&path, legacy).await.unwrap();

        let mut restored = BtCheckpoint::open(&output, true, 8, 4, 2, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0xC0].as_slice()));
        restored.save(&[0xC0], 8).await.unwrap();

        let upgraded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(upgraded.torrent_piece_length(), Some(4));
    }

    #[tokio::test]
    async fn checkpoint_discards_different_torrent_identity() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 8]).unwrap();
        let first_hash = [0x11; 20];
        let second_hash = [0x22; 20];
        let mut checkpoint = BtCheckpoint::open(&output, true, 8, 4, 2, first_hash)
            .await
            .unwrap();
        checkpoint.save(&[0xC0], 4).await.unwrap();

        let mut restored = BtCheckpoint::open(&output, true, 8, 4, 2, second_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0u8].as_slice()));
        assert_eq!(restored.completed_length(), 0);
        restored.save(&[0u8], 0).await.unwrap();
        let control = ControlFile::load(&ControlFile::control_path_for(&output))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(control.torrent_info_hash(), Some(second_hash));
    }

    #[tokio::test]
    async fn checkpoint_rejects_set_trailing_bits() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 10]).unwrap();
        let info_hash = [0x33; 20];
        let mut checkpoint = BtCheckpoint::open(&output, true, 10, 4, 3, info_hash)
            .await
            .unwrap();

        assert!(checkpoint.save(&[0b0010_0001], 4).await.is_err());
        assert_eq!(checkpoint.bitfield(), Some([0u8].as_slice()));
    }

    #[tokio::test]
    async fn checkpoint_discards_sidecar_without_payload() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        let info_hash = [0x44; 20];
        let mut checkpoint = BtCheckpoint::open(&output, false, 8, 4, 2, info_hash)
            .await
            .unwrap();
        checkpoint.save(&[0xC0], 4).await.unwrap();

        let restored = BtCheckpoint::open(&output, false, 8, 4, 2, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0u8].as_slice()));
        assert_eq!(restored.completed_length(), 0);
    }

    #[tokio::test]
    async fn checkpoint_ignores_stale_temporary_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 8]).unwrap();
        let info_hash = [0x77; 20];
        let path = ControlFile::control_path_for(&output);

        let mut checkpoint = BtCheckpoint::open(&output, true, 8, 4, 2, info_hash)
            .await
            .unwrap();
        checkpoint.save(&[0x80], 4).await.unwrap();
        tokio::fs::write(path.with_extension("aria2.tmp"), b"truncated")
            .await
            .unwrap();

        let restored = BtCheckpoint::open(&output, true, 8, 4, 2, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0x80].as_slice()));
        assert!(path.with_extension("aria2.tmp").exists());
    }

    #[tokio::test]
    async fn checkpoint_discards_truncated_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 8]).unwrap();
        let info_hash = [0x88; 20];
        let path = ControlFile::control_path_for(&output);
        tokio::fs::write(&path, b"A2CF\x01\x00").await.unwrap();

        let restored = BtCheckpoint::open(&output, true, 8, 4, 2, info_hash)
            .await
            .unwrap();
        assert_eq!(restored.bitfield(), Some([0u8].as_slice()));
        assert!(
            !path.exists(),
            "stale truncated checkpoint should be removed"
        );
    }

    #[tokio::test]
    async fn checkpoint_save_failure_does_not_advance_memory() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("payload.bin");
        std::fs::write(&output, [0u8; 8]).unwrap();
        let info_hash = [0x99; 20];
        let path = ControlFile::control_path_for(&output);
        let mut checkpoint = BtCheckpoint::open(&output, true, 8, 4, 2, info_hash)
            .await
            .unwrap();
        checkpoint.save(&[0x80], 4).await.unwrap();

        std::fs::remove_file(&output).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(dir.path()).unwrap();
        assert!(checkpoint.save(&[0xC0], 8).await.is_err());
        assert_eq!(checkpoint.bitfield(), Some([0x80].as_slice()));
        assert_eq!(checkpoint.completed_length(), 4);
    }
}
