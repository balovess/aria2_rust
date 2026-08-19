use crate::engine::multi_file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use std::collections::HashMap;

// ======================================================================
// Multi-File Writer
// ======================================================================

/// Coalesced write entry: tracks (file_index, file_offset, data) for batched I/O.
///
/// Adjacent writes to the same file within [`COALESCE_GAP`] bytes are merged
/// into a single larger write, reducing the number of `seek` + `write_all`
/// syscalls.
struct CoalescedWrite {
    file_idx: usize,
    file_offset: u64,
    data: bytes::Bytes,
}

/// Maximum gap (in bytes) between two writes to the same file that will still
/// be coalesced into a single write operation.  Gaps are zero-filled so that
/// the resulting sparse region is correct on disk.
const COALESCE_GAP: u64 = 4096;

/// Writes a completed piece's data across multiple files in a multi-file torrent.
///
/// Handles cross-file boundary cases where a single piece spans multiple files.
/// Each file is opened once, written to at the correct offsets, then flushed.
///
/// # Arguments
/// * `layout` - The multi-file layout defining file boundaries
/// * `piece_idx` - Index of the piece being written
/// * `piece_data` - Complete piece data to write
/// * `_piece_length` - Standard piece length (reserved for future use)
///
/// # Errors
/// Returns error if file open, seek, write, or flush operations fail.
pub async fn write_piece_to_multi_files(
    layout: &MultiFileLayout,
    piece_idx: u32,
    piece_data: &[u8],
    _piece_length: u32,
) -> Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let mut file_writers: HashMap<usize, tokio::fs::File> = HashMap::new();

    let mut data_offset = 0usize;
    while data_offset < piece_data.len() {
        let piece_offset = data_offset as u32;

        if let Some((file_idx, file_offset)) = layout.resolve_file_offset(piece_idx, piece_offset) {
            let file_path = layout
                .file_absolute_path(file_idx)
                .ok_or_else(|| {
                    Aria2Error::Fatal(FatalError::Config("invalid file index".to_string()))
                })?
                .to_path_buf();

            if let std::collections::hash_map::Entry::Vacant(e) = file_writers.entry(file_idx) {
                // NOTE: Do NOT use .truncate(true) — it would destroy existing
                // file content from previously completed pieces (data corruption
                // on partial / resumed downloads).  We seek + write at the
                // correct offset, so the file must be opened for random-access
                // writing without truncation (matching C++ aria2 behavior).
                let f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .read(true)
                    .open(&file_path)
                    .await
                    .map_err(|e| {
                        Aria2Error::Fatal(FatalError::Config(format!("open failed: {}", e)))
                    })?;
                e.insert(f);
            }

            let file_info = layout.get_file_info(file_idx).ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config("invalid file index".to_string()))
            })?;

            let bytes_available_in_file = file_info.length.saturating_sub(file_offset);
            let bytes_remaining_in_piece = (piece_data.len() - data_offset) as u64;
            let write_len = bytes_available_in_file.min(bytes_remaining_in_piece) as usize;

            if write_len == 0 {
                break;
            }

            let chunk = &piece_data[data_offset..data_offset + write_len];

            let writer = file_writers
                .get_mut(&file_idx)
                .expect("file writer was just inserted above and must exist");
            writer
                .seek(std::io::SeekFrom::Start(file_offset))
                .await
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("seek failed: {}", e)))
                })?;
            writer.write_all(chunk).await.map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("write failed: {}", e)))
            })?;

            data_offset += write_len;
        } else {
            break;
        }
    }

    for (_, mut f) in file_writers {
        f.flush()
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("flush failed: {}", e))))?;
    }

    Ok(())
}

// ======================================================================
// Coalesced Multi-File Writer  (Phase 14 / Task I4)
// ======================================================================

/// Writes a completed piece's data across multiple files using **coalesced
/// writes** to reduce the number of `seek` + `write` syscalls.
///
/// # Algorithm
///
/// 1. **Collect** – iterate over `piece_data`, resolve each byte-range to
///    `(file_idx, file_offset)`, and push a raw write entry.
/// 2. **Sort** – order entries by `(file_idx, file_offset)` so that
///    adjacent regions are neighbours.
/// 3. **Coalesce** – merge consecutive writes to the **same file** whose
///    start offset is within [`COALESCE_GAP`] bytes of the previous write's
///    end.  Any gap is zero-filled (sparse region).
/// 4. **Execute** – open each unique file **once**, seek + write_all per
///    coalesced entry, then flush.
///
/// # When to use
///
/// Prefer this function over [`write_piece_to_multi_files`] for production
/// downloads where a piece may span many files or many small write
/// operations would otherwise occur.  The original function is retained for
/// reference and as a simpler implementation.
///
/// # Arguments
/// * `layout`      – The multi-file layout defining file boundaries.
/// * `piece_idx`   – Index of the piece being written.
/// * `piece_data`  – Complete piece data to write.
/// * `_piece_length` – Standard piece length (reserved for future use).
pub async fn write_piece_to_multi_files_coalesced(
    layout: &MultiFileLayout,
    piece_idx: u32,
    piece_data: &bytes::Bytes,
    _piece_length: u32,
) -> Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    // ------------------------------------------------------------------
    // Phase 1: Collect all raw write operations
    // ------------------------------------------------------------------
    // bytes::Bytes::slice() is zero-copy (just bumps the refcount),
    // avoiding a full data clone for each file boundary.
    let mut raw_writes: Vec<(usize, u64, bytes::Bytes)> = Vec::new();
    let mut data_offset = 0usize;

    while data_offset < piece_data.len() {
        let piece_offset = data_offset as u32;
        if let Some((file_idx, file_offset)) = layout.resolve_file_offset(piece_idx, piece_offset) {
            let file_info = layout.get_file_info(file_idx).ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config("invalid file index".to_string()))
            })?;

            let bytes_available = file_info.length.saturating_sub(file_offset);
            let bytes_remaining = (piece_data.len() - data_offset) as u64;
            let write_len = bytes_available.min(bytes_remaining) as usize;

            if write_len > 0 {
                raw_writes.push((
                    file_idx,
                    file_offset,
                    piece_data.slice(data_offset..data_offset + write_len),
                ));
                data_offset += write_len;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // ------------------------------------------------------------------
    // Phase 2: Sort by (file_idx, file_offset)
    // ------------------------------------------------------------------
    raw_writes.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    // ------------------------------------------------------------------
    // Phase 3: Coalesce adjacent writes within COALESCE_GAP
    // ------------------------------------------------------------------
    // For the common case (no coalescing needed), we keep the zero-copy
    // bytes::Bytes slice. When coalescing is required, we switch to
    // BytesMut for efficient concatenation, then freeze back to Bytes.
    let mut coalesced: Vec<CoalescedWrite> = Vec::new();

    for (file_idx, file_offset, data) in raw_writes {
        if let Some(last) = coalesced.last_mut() {
            let last_end = last.file_offset + last.data.len() as u64;
            if last.file_idx == file_idx && file_offset <= last_end + COALESCE_GAP {
                // Need to concatenate — switch to BytesMut for this entry
                let mut buf = bytes::BytesMut::from(last.data.as_ref());
                if file_offset > last_end {
                    // Fill gap with zeros (sparse region)
                    let gap = (file_offset - last_end) as usize;
                    buf.resize(buf.len() + gap, 0u8);
                }
                buf.extend_from_slice(&data);
                last.data = buf.freeze();
                continue;
            }
        }
        coalesced.push(CoalescedWrite {
            file_idx,
            file_offset,
            data,
        });
    }

    // ------------------------------------------------------------------
    // Phase 4: Execute coalesced writes (one open per unique file)
    // ------------------------------------------------------------------
    let mut file_writers: HashMap<usize, tokio::fs::File> = HashMap::new();

    for cw in &coalesced {
        let file_path = layout
            .file_absolute_path(cw.file_idx)
            .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("invalid file index".to_string())))?
            .to_path_buf();

        let writer = match file_writers.entry(cw.file_idx) {
            std::collections::hash_map::Entry::Vacant(e) => {
                // NOTE: Do NOT use .truncate(true) — it would destroy existing
                // file content from previously completed pieces.  We seek +
                // write at the correct offset for random-access writing.
                let f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .read(true)
                    .open(&file_path)
                    .await
                    .map_err(|e| {
                        Aria2Error::Fatal(FatalError::Config(format!("open failed: {}", e)))
                    })?;
                e.insert(f)
            }
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        };

        writer
            .seek(std::io::SeekFrom::Start(cw.file_offset))
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("seek failed: {}", e))))?;
        writer
            .write_all(&cw.data)
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("write failed: {}", e))))?;
    }

    for (_, mut f) in file_writers {
        f.flush()
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("flush failed: {}", e))))?;
    }

    Ok(())
}
