//! RPC data model types.
//!
//! The concrete models are grouped by responsibility while this module keeps
//! the historical public paths stable.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

mod session;
mod status;
mod transfer;

pub type GlobalOptions = Arc<RwLock<HashMap<String, serde_json::Value>>>;
pub type TaskOptions = Arc<RwLock<HashMap<String, HashMap<String, serde_json::Value>>>>;

pub use session::{
    DhtStatus, GlobalStat, SessionInfo, VersionInfo, create_gid, generate_session_id,
};
pub use status::{BittorrentInfo, BittorrentMetaInfo, DownloadStatus, FileInfo, StatusInfo};
pub use transfer::{
    PeerInfo, ServerInfo, ServerInfoIndex, TrackerInfo, UriEntry, UriInfo, UriStatus,
};

#[cfg(test)]
#[path = "types_tests/basic.rs"]
mod basic_tests;
#[cfg(test)]
#[path = "types_tests/session.rs"]
mod session_tests;
#[cfg(test)]
#[path = "types_tests/torrent.rs"]
mod torrent_tests;
#[cfg(test)]
#[path = "types_tests/transfer.rs"]
mod transfer_tests;
#[cfg(test)]
#[path = "types_tests/wire.rs"]
mod wire_tests;
