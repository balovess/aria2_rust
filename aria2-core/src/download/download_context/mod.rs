//! Download context — central metadata binding file entries, URIs, and download metadata.
//!
//! Equivalent to the C++ aria2 `DownloadContext` class. This is the primary
//! data object associated with a single download task, holding:
//!
//! - **File entries** — ordered list of files (single for HTTP, multi for torrent/metalink)
//! - **Piece hashes** — per-piece hash values for verification
//! - **Whole-file checksum** — digest and algorithm for full-file verification
//! - **Network stats** — per-download speed / byte counters
//! - **Attributes** — typed extension map (BitTorrent, Ed2k, etc.)
//! - **Signature** — optional Metalink/PGP signature
//!
//! # Design differences from C++ aria2
//!
//! | C++ aria2 | Rust | Rationale |
//! |---|---|---|
//! | `RequestGroup*` raw pointer | `owner_request_group_id: Option<u64>` | No raw pointers; ID-based reference |
//! | `vector<shared_ptr<ContextAttribute>>` fixed-size | `HashMap<ContextAttributeType, Box<dyn Any + Send + Sync>>` | More flexible, Rust-idiomatic; thread-safe |
//! | `Timer` / `wallclock` | `Instant` | Standard library monotonic clock |
//! | `A2STR::NIL` for missing piece hash | Returns `""` via static | Same semantics, zero-allocation |

mod context;
mod net_stat;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types to preserve the original public API.
pub use context::DownloadContext;
pub use net_stat::NetStat;
pub use types::{BtFileMode, ContextAttributeType, Signature, TorrentAttribute};
