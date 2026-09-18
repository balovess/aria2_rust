//! Sequential file pre-allocation queue used by download commands.
//!
//! The module owns queue state and dispatches one allocation at a time. The
//! actual platform-specific allocation strategies remain in
//! [`crate::filesystem::file_allocation`].

use crate::error::Aria2Error;

mod dispatch;
mod helpers;
mod queue;

#[cfg(test)]
mod tests;

pub use dispatch::{SharedFileAllocationMan, shared};
pub(crate) use helpers::cancel_gid;
pub use helpers::{enqueue_multi, enqueue_path};
#[cfg(test)]
pub(crate) use queue::FileAllocationEntry;
pub use queue::FileAllocationMan;

/// Error reported when an allocation is cancelled by engine cleanup.
pub(super) fn cancelled_error() -> Aria2Error {
    Aria2Error::DownloadFailed("file allocation cancelled".to_string())
}
