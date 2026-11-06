use crate::filesystem::control_file::ControlFileInFlightPiece;

const IN_FLIGHT_BLOCK_SIZE: u64 = 16 * 1024;

/// Tracks a contiguous, written prefix within each HTTP parent piece.
pub(super) struct InFlightProgress {
    total_length: u64,
    piece_length: u64,
    completed_prefixes: Vec<u64>,
}

impl InFlightProgress {
    pub(super) fn new(total_length: u64, piece_length: u64, piece_count: usize) -> Self {
        Self {
            total_length,
            piece_length,
            completed_prefixes: vec![0; piece_count],
        }
    }

    /// Restore only a contiguous run of complete 16 KiB blocks from records
    /// whose geometry matches the current HTTP layout.
    pub(super) fn restore(
        &mut self,
        pieces: &[ControlFileInFlightPiece],
        completed_bitfield: &[u8],
    ) -> Vec<(u32, u64)> {
        let mut seen = vec![false; self.completed_prefixes.len()];
        let mut restored = Vec::new();
        for piece in pieces {
            let index = piece.index as usize;
            let Some(prefix) = self.valid_prefix(piece, completed_bitfield, &mut seen) else {
                continue;
            };
            self.completed_prefixes[index] = prefix;
            restored.push((piece.index, prefix));
        }
        restored
    }

    fn valid_prefix(
        &self,
        piece: &ControlFileInFlightPiece,
        completed_bitfield: &[u8],
        seen: &mut [bool],
    ) -> Option<u64> {
        let index = usize::try_from(piece.index).ok()?;
        let already_completed = completed_bitfield
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0);
        if index >= self.completed_prefixes.len() || seen[index] || already_completed {
            return None;
        }
        seen[index] = true;

        let piece_offset = u64::from(piece.index).checked_mul(self.piece_length)?;
        let expected_length = self
            .total_length
            .checked_sub(piece_offset)?
            .min(self.piece_length);
        if u64::from(piece.length) != expected_length {
            return None;
        }

        let block_count = usize::try_from(expected_length.div_ceil(IN_FLIGHT_BLOCK_SIZE)).ok()?;
        let expected_bitfield_len = block_count.div_ceil(8);
        if piece.bitfield.len() != expected_bitfield_len {
            return None;
        }
        let unused_bits = (8 - block_count % 8) % 8;
        if unused_bits != 0
            && piece
                .bitfield
                .last()
                .is_some_and(|byte| byte & ((1u8 << unused_bits) - 1) != 0)
        {
            return None;
        }

        let mut blocks = 0usize;
        while blocks < block_count {
            let set = piece.bitfield[blocks / 8] & (1 << (7 - blocks % 8)) != 0;
            if !set {
                break;
            }
            blocks += 1;
        }
        if blocks == 0 {
            return None;
        }

        let prefix = if blocks == block_count {
            expected_length
        } else {
            (blocks as u64).saturating_mul(IN_FLIGHT_BLOCK_SIZE)
        };
        Some(prefix)
    }

    /// Record bytes only when they extend or overlap the current contiguous
    /// prefix. HTTP range responses are sequential within one parent piece.
    pub(super) fn record_range(&mut self, offset: u64, length: u64) {
        let Some(mut end) = offset.checked_add(length) else {
            return;
        };
        end = end.min(self.total_length);
        if offset >= end || self.piece_length == 0 {
            return;
        }

        let mut cursor = offset;
        while cursor < end {
            let index = (cursor / self.piece_length) as usize;
            let Some(prefix) = self.completed_prefixes.get_mut(index) else {
                return;
            };
            let piece_offset = (index as u64).saturating_mul(self.piece_length);
            let piece_end = piece_offset
                .saturating_add(self.piece_length)
                .min(self.total_length);
            let local_offset = cursor.saturating_sub(piece_offset);
            if local_offset > *prefix {
                return;
            }
            let written_end = end.min(piece_end);
            *prefix = (*prefix).max(written_end.saturating_sub(piece_offset));
            cursor = written_end;
        }
    }

    /// Written prefixes of the pieces `is_complete` rejects, as
    /// `(offset, length)`, so a fallback does not fetch (and on failure
    /// zero-fill) bytes that the checkpoint already claims.
    pub(super) fn partial_ranges(&self, is_complete: impl Fn(usize) -> bool) -> Vec<(u64, u64)> {
        self.completed_prefixes
            .iter()
            .enumerate()
            .filter(|&(index, &prefix)| prefix > 0 && !is_complete(index))
            .map(|(index, &prefix)| ((index as u64).saturating_mul(self.piece_length), prefix))
            .collect()
    }

    /// Build canonical checkpoint records. Only complete blocks in each
    /// contiguous prefix are advertised as reusable.
    pub(super) fn snapshot(&self, completed_bitfield: &[u8]) -> Vec<ControlFileInFlightPiece> {
        self.completed_prefixes
            .iter()
            .enumerate()
            .filter_map(|(index, &prefix)| {
                let already_completed = completed_bitfield
                    .get(index / 8)
                    .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0);
                if prefix == 0 || already_completed {
                    return None;
                }
                let piece_offset = (index as u64).checked_mul(self.piece_length)?;
                let length = self
                    .total_length
                    .checked_sub(piece_offset)?
                    .min(self.piece_length);
                let prefix = prefix.min(length);
                let block_count = usize::try_from(length.div_ceil(IN_FLIGHT_BLOCK_SIZE)).ok()?;
                let complete_blocks = if prefix == length {
                    block_count
                } else {
                    usize::try_from(prefix / IN_FLIGHT_BLOCK_SIZE).ok()?
                };
                if complete_blocks == 0 {
                    return None;
                }
                let mut bitfield = vec![0; block_count.div_ceil(8)];
                for block in 0..complete_blocks {
                    bitfield[block / 8] |= 1 << (7 - block % 8);
                }
                Some(ControlFileInFlightPiece {
                    index: u32::try_from(index).ok()?,
                    length: u32::try_from(length).ok()?,
                    bitfield,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{IN_FLIGHT_BLOCK_SIZE, InFlightProgress};
    use crate::filesystem::control_file::ControlFileInFlightPiece;

    #[test]
    fn snapshots_only_complete_blocks_from_contiguous_written_prefixes() {
        let piece_length = 64 * 1024;
        let mut progress = InFlightProgress::new(piece_length, piece_length, 1);
        progress.record_range(0, 2 * IN_FLIGHT_BLOCK_SIZE + IN_FLIGHT_BLOCK_SIZE / 2);

        let pieces = progress.snapshot(&[]);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].length, piece_length as u32);
        assert_eq!(pieces[0].bitfield, vec![0b1100_0000]);
    }

    #[test]
    fn snapshots_only_a_contiguous_prefix_and_handles_a_short_final_piece() {
        let mut progress = InFlightProgress::new(20 * 1024, 16 * 1024, 2);
        progress.record_range(16 * 1024, 4 * 1024);

        let final_piece = progress.snapshot(&[]);
        assert_eq!(final_piece.len(), 1);
        assert_eq!(final_piece[0].index, 1);
        assert_eq!(final_piece[0].length, 4 * 1024);
        assert_eq!(final_piece[0].bitfield, vec![0b1000_0000]);

        progress.record_range(8 * 1024, 16 * 1024);
        assert_eq!(progress.snapshot(&[]), final_piece);

        progress.record_range(0, 16 * 1024);
        let pieces = progress.snapshot(&[]);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].index, 0);
        assert_eq!(pieces[0].bitfield, vec![0b1000_0000]);
    }

    #[test]
    fn restores_only_valid_leading_blocks_and_skips_completed_or_bad_geometry() {
        let mut progress = InFlightProgress::new(160 * 1024, 64 * 1024, 3);
        let pieces = [
            ControlFileInFlightPiece {
                index: 0,
                length: 64 * 1024,
                bitfield: vec![0b1000_0000],
            },
            ControlFileInFlightPiece {
                index: 1,
                length: 64 * 1024,
                bitfield: vec![0b1010_0000],
            },
            ControlFileInFlightPiece {
                index: 1,
                length: 64 * 1024,
                bitfield: vec![0b1000_0000],
            },
            ControlFileInFlightPiece {
                index: 2,
                length: 31 * 1024,
                bitfield: vec![0b1000_0000],
            },
            ControlFileInFlightPiece {
                index: 3,
                length: 64 * 1024,
                bitfield: vec![0b1000_0000],
            },
        ];

        let restored = progress.restore(&pieces, &[0b1000_0000]);

        assert_eq!(restored, vec![(1, IN_FLIGHT_BLOCK_SIZE)]);
    }
}
