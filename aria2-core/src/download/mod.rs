//! Download domain objects: per-file tracking, download context, and per-connection request state.
//!
//! - [`download_context`] — `DownloadContext` (central metadata: file entries, hashes, stats, attributes)
//! - [`file_entry`] — `FileEntry` (per-file URI + request management)
//! - [`request`] — `Request` (per-connection request state, `PeerStat`)

pub mod download_context;
pub mod file_entry;
pub mod request;

pub use download_context::{
    BtFileMode, ContextAttributeType, DownloadContext, NetStat, Signature, TorrentAttribute,
};
