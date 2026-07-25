mod status;
mod group_id;
mod options;
mod progress;
mod group;
#[cfg(test)]
mod tests;

// Re-export all public types so the external API remains unchanged.
// Import paths like `crate::request::request_group::RequestGroup` still work.
pub use status::DownloadStatus;
pub use group_id::GroupId;
pub use options::{
    ChangeableKind, DownloadOptions, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS,
    RUNTIME_CHANGEABLE_OPTIONS, is_option_changeable,
};
pub use progress::AtomicProgress;
pub use group::RequestGroup;
