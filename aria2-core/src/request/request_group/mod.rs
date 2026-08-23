mod activity;
mod bt_peer_snapshot;
mod connection_state;
mod dependency;
pub mod download_result;
mod group;
mod group_id;
mod halt_reason;
mod options;
mod progress;
pub mod result_code;
mod status;
mod status_snapshot;

// Sub-modules with impl blocks split from group.rs for the 600-line limit.
mod context_ops;
mod control_ops;
mod file_lifecycle;
mod lifecycle_ops;
mod metadata_info;
mod metadata_ops;
mod options_ops;
mod progress_ops;
mod result_ops;

mod tests;

// Re-export all public types so the external API remains unchanged.
// Import paths like `crate::request::request_group::RequestGroup` still work.
pub use crate::config::runtime::{
    ChangeableKind, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
    is_option_changeable,
};
pub use activity::ActivitySignal;
pub use bt_peer_snapshot::BtPeerSnapshot;
pub(crate) use connection_state::{ActiveConnectionGuard, BtConnectionGuard, ConnectionState};
#[cfg(feature = "bittorrent")]
pub use dependency::BtDependencyResolution;
#[cfg(feature = "bittorrent")]
pub use dependency::{BtDependency, BtFileMapping};
pub use dependency::{CompletionDependency, Dependency, NoDependency};
pub use download_result::{DownloadResult, FileEntry, UriEntry};
pub use group::RequestGroup;
pub use group_id::GroupId;
pub use halt_reason::{DownloadControlFlags, HaltReason};
pub use metadata_info::MetadataInfo;
pub use options::{DEFAULT_DISK_CACHE_BYTES, DownloadOptions, FollowMode, option_value_to_string};
pub use progress::AtomicProgress;
pub use result_code::DownloadResultCode;
pub use status::DownloadStatus;
pub use status_snapshot::{BtStatusSnapshot, DownloadStatusSnapshot};
