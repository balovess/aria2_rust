//! Sequential, size-checked reads from an SFTP file.

use super::super::file_ops::{FileOpError, OpenFlags, SftpFileOps, SftpRemoteFile};
use super::{MAX_BUFFER_SIZE, MIN_BUFFER_SIZE};

/// Failure returned while reading the next size-bounded SFTP chunk.
#[derive(Debug, thiserror::Error)]
pub enum SftpDownloadReadError {
    #[error(transparent)]
    Remote(#[from] FileOpError),
    #[error("remote file ended at offset {offset} before its advertised size {total_size}")]
    UnexpectedEof { offset: u64, total_size: u64 },
}

/// Reads a known-size SFTP file in bounded chunks.
///
/// The caller owns the destination and its progress/checkpoint policy. This
/// type owns the protocol-side offset, read size, premature-EOF check, and
/// remote handle cleanup.
pub struct SftpDownloadReader<'a> {
    remote_file: SftpRemoteFile<'a>,
    total_size: u64,
    offset: u64,
    chunk_size: usize,
}

impl<'a> SftpDownloadReader<'a> {
    /// Open a known-size remote file for sequential reads.
    pub async fn open(
        ops: &SftpFileOps<'a>,
        remote_path: &str,
        total_size: u64,
        start_offset: u64,
        chunk_size: usize,
    ) -> Result<Self, FileOpError> {
        if start_offset > total_size {
            return Err(FileOpError::Other {
                message: format!(
                    "SFTP start offset {start_offset} exceeds remote file size {total_size}"
                ),
            });
        }

        let remote_file = ops.open(remote_path, OpenFlags::readonly(), 0).await?;

        Ok(Self {
            remote_file,
            total_size,
            offset: start_offset,
            chunk_size: chunk_size.clamp(MIN_BUFFER_SIZE, MAX_BUFFER_SIZE),
        })
    }

    /// The offset of the next protocol read.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read the next chunk, returning `None` once the advertised size is read.
    /// An empty DATA/EOF response before that size is a protocol transfer error.
    pub async fn read_next(&mut self) -> Result<Option<Vec<u8>>, SftpDownloadReadError> {
        let remaining = self.total_size.saturating_sub(self.offset);
        if remaining == 0 {
            return Ok(None);
        }

        let requested = (self.chunk_size as u64).min(remaining) as u32;
        let data = self.remote_file.read_at(self.offset, requested).await?;
        if data.is_empty() {
            return Err(SftpDownloadReadError::UnexpectedEof {
                offset: self.offset,
                total_size: self.total_size,
            });
        }

        self.offset += data.len() as u64;
        Ok(Some(data))
    }

    /// Close the remote handle. Call this before dropping the reader.
    pub async fn close(&mut self) -> Result<(), FileOpError> {
        self.remote_file.close().await
    }
}
