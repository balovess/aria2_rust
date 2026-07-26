//! HTTP write buffer for pipelined request batching.
//!
//! Mirrors C++ aria2's `SocketBuffer` class, providing:
//! - Queued outbound data entries (strings or byte vectors)
//! - Batched flush via a single write syscall
//! - Partial-write tracking (offset into front entry)
//! - `is_empty()` for event loop integration
//!
//! Unlike the C++ version which uses `writev()` (scatter-gather I/O),
//! this Rust implementation coalesces entries into a single contiguous
//! buffer before writing, which is more compatible with Tokio's async
//! write model.

use std::collections::VecDeque;
use std::io;

use tokio::io::AsyncWriteExt;

/// A single entry in the write buffer queue.
#[derive(Debug)]
enum BufferEntry {
    /// A UTF-8 string (e.g., an HTTP request).
    String { data: String, offset: usize },
    /// Raw bytes (e.g., a POST body).
    Bytes { data: Vec<u8>, offset: usize },
}

impl BufferEntry {
    /// Returns the remaining (unsent) bytes of this entry.
    fn remaining(&self) -> &[u8] {
        match self {
            BufferEntry::String { data, offset } => &data.as_bytes()[*offset..],
            BufferEntry::Bytes { data, offset } => &data[*offset..],
        }
    }

    /// Advance the offset by `n` bytes. Returns `true` if the entry is fully consumed.
    fn advance(&mut self, n: usize) -> bool {
        match self {
            BufferEntry::String { data, offset } => {
                *offset += n;
                *offset >= data.len()
            }
            BufferEntry::Bytes { data, offset } => {
                *offset += n;
                *offset >= data.len()
            }
        }
    }

    /// Total remaining bytes in this entry.
    fn remaining_len(&self) -> usize {
        self.remaining().len()
    }
}

/// Maximum total bytes to coalesce for a single write syscall.
/// Matches C++ aria2's 24 KiB threshold.
const MAX_COALESCE_SIZE: usize = 24 * 1024;

/// HTTP write buffer for pipelined request batching.
///
/// Usage:
/// 1. Call `push_str()` or `push_bytes()` to enqueue outbound data
/// 2. Call `flush()` when the socket is writable to write queued data
/// 3. Check `is_empty()` to determine if write-interest should be registered
#[derive(Debug)]
pub struct HttpWriteBuffer {
    /// Queue of pending write entries.
    entries: VecDeque<BufferEntry>,
    /// Total bytes remaining across all entries.
    total_remaining: usize,
}

impl HttpWriteBuffer {
    /// Create a new empty write buffer.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            total_remaining: 0,
        }
    }

    /// Enqueue a string for writing (typically an HTTP request).
    pub fn push_str(&mut self, data: String) {
        let len = data.len();
        if len > 0 {
            self.total_remaining += len;
            self.entries
                .push_back(BufferEntry::String { data, offset: 0 });
        }
    }

    /// Enqueue raw bytes for writing (e.g., POST body).
    pub fn push_bytes(&mut self, data: Vec<u8>) {
        let len = data.len();
        if len > 0 {
            self.total_remaining += len;
            self.entries
                .push_back(BufferEntry::Bytes { data, offset: 0 });
        }
    }

    /// Returns `true` if all queued data has been written.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the total number of bytes remaining to be written.
    pub fn remaining_bytes(&self) -> usize {
        self.total_remaining
    }

    /// Coalesce up to `MAX_COALESCE_SIZE` bytes from the queue into a single
    /// contiguous buffer, then write it to `writer`.
    ///
    /// Returns the number of bytes written on success.
    /// Fully consumed entries are removed from the queue.
    /// Partially written entries have their offset advanced.
    pub async fn flush<W: tokio::io::AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
    ) -> io::Result<usize> {
        if self.entries.is_empty() {
            return Ok(0);
        }

        // Coalesce entries into a single buffer up to MAX_COALESCE_SIZE
        let mut coalesced = Vec::with_capacity(MAX_COALESCE_SIZE.min(self.total_remaining));

        for entry in &self.entries {
            let remaining = entry.remaining();
            if coalesced.len() + remaining.len() > MAX_COALESCE_SIZE {
                // Would exceed the coalesce limit — take a prefix
                let available = MAX_COALESCE_SIZE - coalesced.len();
                if available > 0 {
                    coalesced.extend_from_slice(&remaining[..available]);
                }
                break;
            }
            coalesced.extend_from_slice(remaining);
        }

        if coalesced.is_empty() {
            return Ok(0);
        }

        // Perform a single write
        let written = writer.write(&coalesced).await?;

        if written == 0 {
            return Ok(0);
        }

        // Advance offsets in the queue entries
        let mut remaining_to_advance = written;
        while remaining_to_advance > 0 && !self.entries.is_empty() {
            let entry = self.entries.front_mut().unwrap();
            let entry_remaining = entry.remaining_len();

            if entry_remaining <= remaining_to_advance {
                // This entry is fully consumed
                remaining_to_advance -= entry_remaining;
                self.total_remaining -= entry_remaining;
                self.entries.pop_front();
            } else {
                // Partial write within this entry
                entry.advance(remaining_to_advance);
                self.total_remaining -= remaining_to_advance;
                remaining_to_advance = 0;
            }
        }

        Ok(written)
    }
}

impl Default for HttpWriteBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_entry_remaining_and_advance() {
        let mut entry = BufferEntry::String {
            data: "hello world".to_string(),
            offset: 0,
        };
        assert_eq!(entry.remaining(), b"hello world");
        assert_eq!(entry.remaining_len(), 11);

        assert!(!entry.advance(5)); // not fully consumed
        assert_eq!(entry.remaining(), b" world");
        assert_eq!(entry.remaining_len(), 6);

        assert!(entry.advance(6)); // fully consumed
        assert_eq!(entry.remaining_len(), 0);
    }

    #[test]
    fn test_buffer_entry_bytes_remaining_and_advance() {
        let mut entry = BufferEntry::Bytes {
            data: vec![1, 2, 3, 4, 5],
            offset: 0,
        };
        assert_eq!(entry.remaining(), &[1, 2, 3, 4, 5]);

        assert!(!entry.advance(2));
        assert_eq!(entry.remaining(), &[3, 4, 5]);

        assert!(entry.advance(3));
        assert_eq!(entry.remaining_len(), 0);
    }

    #[test]
    fn test_push_str_and_bytes() {
        let mut buf = HttpWriteBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.remaining_bytes(), 0);

        buf.push_str("GET / HTTP/1.1\r\n\r\n".to_string());
        assert!(!buf.is_empty());
        assert_eq!(buf.remaining_bytes(), 18);

        buf.push_bytes(vec![1, 2, 3]);
        assert_eq!(buf.remaining_bytes(), 21);
    }

    #[test]
    fn test_push_empty_data_is_noop() {
        let mut buf = HttpWriteBuffer::new();
        buf.push_str(String::new());
        buf.push_bytes(Vec::new());
        assert!(buf.is_empty());
        assert_eq!(buf.remaining_bytes(), 0);
    }

    #[tokio::test]
    async fn test_flush_writes_all_data() {
        let mut buf = HttpWriteBuffer::new();
        buf.push_str("hello ".to_string());
        buf.push_str("world".to_string());

        let mut sink = Vec::new();
        let written = buf.flush(&mut sink).await.unwrap();
        assert_eq!(written, 11);
        assert_eq!(&sink[..], b"hello world");
        assert!(buf.is_empty());
        assert_eq!(buf.remaining_bytes(), 0);
    }

    #[tokio::test]
    async fn test_flush_partial_write() {
        use std::io::Write;

        // A writer that only accepts 5 bytes at a time
        struct PartialWriter {
            inner: Vec<u8>,
            limit: usize,
        }

        impl Write for PartialWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let n = self.limit.min(buf.len());
                self.inner.extend_from_slice(&buf[..n]);
                Ok(n)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl tokio::io::AsyncWrite for PartialWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(std::io::Write::write(self.get_mut(), buf))
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let mut buf = HttpWriteBuffer::new();
        buf.push_str("hello world!".to_string()); // 12 bytes

        let mut writer = PartialWriter {
            inner: Vec::new(),
            limit: 5,
        };

        let written = buf.flush(&mut writer).await.unwrap();
        assert_eq!(written, 5);
        assert_eq!(&writer.inner[..], b"hello");
        assert!(!buf.is_empty());
        assert_eq!(buf.remaining_bytes(), 7);

        // Second flush
        let written = buf.flush(&mut writer).await.unwrap();
        assert_eq!(written, 5);
        assert_eq!(&writer.inner[..], b"hello worl");
        assert!(!buf.is_empty());
        assert_eq!(buf.remaining_bytes(), 2);

        // Third flush
        let written = buf.flush(&mut writer).await.unwrap();
        assert_eq!(written, 2);
        assert_eq!(&writer.inner[..], b"hello world!");
        assert!(buf.is_empty());
        assert_eq!(buf.remaining_bytes(), 0);
    }

    #[tokio::test]
    async fn test_flush_empty_buffer() {
        let mut buf = HttpWriteBuffer::new();
        let mut sink = Vec::new();
        let written = buf.flush(&mut sink).await.unwrap();
        assert_eq!(written, 0);
        assert!(sink.is_empty());
    }

    #[tokio::test]
    async fn test_flush_multiple_entries_coalesced() {
        let mut buf = HttpWriteBuffer::new();
        buf.push_str("GET /a ".to_string());
        buf.push_bytes(vec![b'X', b'Y']);
        buf.push_str("HTTP/1.1\r\n".to_string());

        let mut sink = Vec::new();
        let written = buf.flush(&mut sink).await.unwrap();
        assert_eq!(written, 19);
        assert!(buf.is_empty());
        assert_eq!(&sink[..], b"GET /a XYHTTP/1.1\r\n");
    }

    #[tokio::test]
    async fn test_remaining_bytes_tracks_partial_writes() {
        let mut buf = HttpWriteBuffer::new();
        buf.push_str("ABCDEFGHIJ".to_string()); // 10 bytes
        assert_eq!(buf.remaining_bytes(), 10);

        let mut sink = Vec::new();
        // Vec::write always writes everything, so all 10 bytes go out
        let written = buf.flush(&mut sink).await.unwrap();
        assert_eq!(written, 10);
        assert_eq!(buf.remaining_bytes(), 0);
    }
}
