mod group;
mod group_id;
mod halt_reason;
mod options;
mod progress;
mod result_code;
mod status;
#[cfg(test)]
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
pub use result_code::{DownloadResult, DownloadResultCode};
pub use status::DownloadStatus;
