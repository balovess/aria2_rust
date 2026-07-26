mod group;
mod group_id;
mod halt_reason;
mod options;
mod progress;
pub mod result_code;
mod status;
pub mod download_result;
mod dependency;

// Sub-modules with impl blocks split from group.rs for the 600-line limit.
mod lifecycle_ops;
mod file_lifecycle;
mod context_ops;
mod progress_ops;
mod options_ops;
mod control_ops;
mod result_ops;

mod tests;

// Re-export all public types so the external API remains unchanged.
// Import paths like `crate::request::request_group::RequestGroup` still work.
pub use group::RequestGroup;
pub use group_id::GroupId;
pub use halt_reason::{DownloadControlFlags, HaltReason};
pub use options::{
    ChangeableKind, DownloadOptions, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS,
    RUNTIME_CHANGEABLE_OPTIONS, is_option_changeable,
};
pub use progress::AtomicProgress;
pub use result_code::DownloadResultCode;
pub use download_result::{DownloadResult, FileEntry, UriEntry};
pub use dependency::{CompletionDependency, Dependency, NoDependency};
pub use status::DownloadStatus;
