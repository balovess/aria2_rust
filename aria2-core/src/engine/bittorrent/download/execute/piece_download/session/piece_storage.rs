use std::collections::{HashMap, HashSet};

use crate::engine::bittorrent::peer::message_handler::types::{BLOCK_SIZE, ReceivedPieceBlock};
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFileInFlightPiece;
use crate::filesystem::disk_writer::SeekableDiskWriter;

use super::PieceDownloadSession;

pub(super) async fn stage_received_block(
    writer: &mut Box<dyn SeekableDiskWriter>,
    layout: Option<&crate::engine::bittorrent::torrent::file_layout::MultiFileLayout>,
    piece_lengths: &HashMap<u32, u32>,
    in_flight: &mut HashMap<u32, ControlFileInFlightPiece>,
    dirty_multi_file_indices: &mut HashSet<usize>,
    max_open_files: usize,
    block: ReceivedPieceBlock,
) -> Result<()> {
    let Some(&piece_length) = piece_lengths.get(&block.piece_index) else {
        return Ok(());
    };
    let block_count = piece_length.div_ceil(BLOCK_SIZE);
    let expected_offset = block.block_index.saturating_mul(BLOCK_SIZE);
    let expected_length = (piece_length.saturating_sub(expected_offset)).min(BLOCK_SIZE);
    if block.block_index >= block_count
        || block.offset != expected_offset
        || block.data.len() != expected_length as usize
    {
        return Err(Aria2Error::Network(format!(
            "Invalid received block layout for piece {} block {}",
            block.piece_index, block.block_index
        )));
    }

    let piece_index = block.piece_index;
    let block_index = block.block_index;
    let offset = block.offset;
    if let Some(layout) = layout {
        let touched_files =
            crate::engine::bittorrent::piece::downloader::write_piece_block_to_multi_files(
                layout,
                piece_index,
                offset,
                &block.data,
                max_open_files,
            )
            .await?;
        dirty_multi_file_indices.extend(touched_files);
    } else {
        let global_offset = u64::from(piece_index) * u64::from(piece_length) + u64::from(offset);
        writer.write_bytes_at(global_offset, block.data).await?;
    }

    // These blocks are staged until the complete piece passes its hash. The
    // in-flight bitmap is only written to a checkpoint after its payload has
    // crossed the stable-storage barrier in the checkpoint path.
    let bitfield_len = (block_count as usize).div_ceil(8);
    let record = in_flight
        .entry(piece_index)
        .or_insert_with(|| ControlFileInFlightPiece {
            index: piece_index,
            length: piece_length,
            bitfield: vec![0; bitfield_len],
        });
    if record.length != piece_length || record.bitfield.len() != bitfield_len {
        *record = ControlFileInFlightPiece {
            index: piece_index,
            length: piece_length,
            bitfield: vec![0; bitfield_len],
        };
    }
    record.bitfield[block_index as usize / 8] |= 1 << (7 - block_index % 8);
    Ok(())
}

pub(super) fn in_flight_snapshot(
    in_flight: &HashMap<u32, ControlFileInFlightPiece>,
) -> Vec<ControlFileInFlightPiece> {
    let mut pieces = in_flight.values().cloned().collect::<Vec<_>>();
    pieces.sort_unstable_by_key(|piece| piece.index);
    pieces
}

impl PieceDownloadSession<'_> {
    pub(super) async fn load_resumed_blocks(
        &mut self,
        piece_index: u32,
        piece_length: u32,
    ) -> Result<Vec<Option<bytes::Bytes>>> {
        let block_count = piece_length.div_ceil(BLOCK_SIZE) as usize;
        let mut blocks = vec![None; block_count];
        let Some(mut record) = self.in_flight_pieces.get(&piece_index).cloned() else {
            return Ok(blocks);
        };
        let expected_bitfield_len = block_count.div_ceil(8);
        if self.piece_picker.is_completed(piece_index)
            || record.length != piece_length
            || record.bitfield.len() != expected_bitfield_len
        {
            self.in_flight_pieces.remove(&piece_index);
            return Ok(blocks);
        }

        for (block_index, block) in blocks.iter_mut().enumerate() {
            let mask = 1 << (7 - block_index % 8);
            if record.bitfield[block_index / 8] & mask == 0 {
                continue;
            }
            let offset = block_index as u32 * BLOCK_SIZE;
            let length = (piece_length - offset).min(BLOCK_SIZE) as usize;
            let bytes = if let Some(layout) = self.command.multi_file_layout.as_ref() {
                crate::engine::bittorrent::piece::downloader::read_piece_range_from_files(
                    layout,
                    piece_index,
                    offset,
                    length as u32,
                )
                .await
            } else {
                let mut data = vec![0; length];
                let mut read = 0;
                while read < data.len() {
                    match self
                        .writer
                        .read_at(
                            u64::from(piece_index) * u64::from(piece_length)
                                + u64::from(offset)
                                + read as u64,
                            &mut data[read..],
                        )
                        .await
                    {
                        Ok(0) | Err(_) => break,
                        Ok(bytes_read) => read += bytes_read,
                    }
                }
                (read == data.len()).then_some(data)
            };
            if let Some(bytes) = bytes {
                *block = Some(bytes::Bytes::from(bytes));
            } else {
                record.bitfield[block_index / 8] &= !mask;
            }
        }

        if record.bitfield.iter().all(|byte| *byte == 0) {
            self.in_flight_pieces.remove(&piece_index);
        } else {
            self.in_flight_pieces.insert(piece_index, record);
        }
        Ok(blocks)
    }
}
