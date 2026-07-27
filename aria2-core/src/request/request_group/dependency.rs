//! Download dependency resolution.
//!
//! Mirrors C++ `Dependency` class hierarchy. A `Dependency` represents a
//! condition that must be satisfied before a `RequestGroup` can be promoted
//! from reserved to active. The most common dependency is "wait for another
//! download to finish" (e.g. Metalink → torrent download chains).

use std::sync::Arc;

use super::GroupId;

/// A dependency that must be resolved before a download can start.
///
/// Mirrors C++ `Dependency` base class with `virtual bool resolve()`.
/// In Rust we use a trait object so each dependency type can define
/// its own resolution logic.
///
/// The trait requires `Any` support to enable downcasting in the engine
/// loop (e.g. finding `CompletionDependency` instances to resolve them
/// when their prerequisite group completes).
pub trait Dependency: Send + Sync + std::fmt::Debug + std::any::Any {
    /// Check whether this dependency has been resolved.
    ///
    /// Returns `true` if the dependency is satisfied and the download
    /// can proceed, `false` if it must remain in the reserved queue.
    fn resolve(&self) -> bool;

    /// Human-readable description of this dependency for logging.
    fn description(&self) -> String;

    /// Support for downcasting. Required for the engine loop to find
    /// specific dependency types (e.g. `CompletionDependency`) in the
    /// reserved queue.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Dependency on another download completing.
///
/// The dependent download waits in the reserved queue until the
/// prerequisite download finishes. This is used by:
/// - Metalink: parent Metalink download → child torrent downloads
/// - Torrent→magnet: magnet link download triggers torrent download
///
/// Mirrors C++ `DownloadResultDependency` and `GIDDependency`.
#[derive(Debug)]
pub struct CompletionDependency {
    /// GID of the prerequisite download.
    pub depends_on_gid: GroupId,
    /// Shared flag that gets set when the prerequisite completes.
    /// Allows lock-free resolution checking from the promotion path.
    completed: Arc<std::sync::atomic::AtomicBool>,
}

impl CompletionDependency {
    /// Create a new completion dependency on the given GID.
    ///
    /// The `completed` flag starts as `false` and should be set to `true`
    /// by the engine loop when the prerequisite group is demoted to stopped.
    pub fn new(depends_on_gid: GroupId) -> Self {
        Self {
            depends_on_gid,
            completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Get a shared reference to the completion flag.
    ///
    /// The engine loop uses this to mark the dependency as resolved
    /// when the prerequisite download finishes.
    pub fn completed_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.completed)
    }

    /// Manually mark this dependency as resolved (for testing).
    pub fn mark_resolved(&self) {
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl Dependency for CompletionDependency {
    fn resolve(&self) -> bool {
        self.completed.load(std::sync::atomic::Ordering::Acquire)
    }

    fn description(&self) -> String {
        format!(
            "Waiting for download #{} to complete",
            self.depends_on_gid.to_hex_string()
        )
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A dependency that is always resolved (no-op).
///
/// Used as a default when a group has no dependencies.
#[derive(Debug)]
pub struct NoDependency;

impl Dependency for NoDependency {
    fn resolve(&self) -> bool {
        true
    }

    fn description(&self) -> String {
        "No dependency".to_string()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_dependency_always_resolved() {
        let dep = NoDependency;
        assert!(dep.resolve());
        assert_eq!(dep.description(), "No dependency");
    }

    #[test]
    fn test_completion_dependency_initially_unresolved() {
        let dep = CompletionDependency::new(GroupId::new(1));
        assert!(!dep.resolve());
        assert!(dep.description().contains("#0000000000000001"));
    }

    #[test]
    fn test_completion_dependency_resolved_after_mark() {
        let dep = CompletionDependency::new(GroupId::new(42));
        assert!(!dep.resolve());

        dep.mark_resolved();
        assert!(dep.resolve());
    }

    #[test]
    fn test_completion_dependency_shared_flag() {
        let dep = CompletionDependency::new(GroupId::new(1));
        let flag = dep.completed_flag();

        assert!(!dep.resolve());

        // Setting the flag from a different reference resolves the dependency.
        flag.store(true, std::sync::atomic::Ordering::Release);
        assert!(dep.resolve());
    }
}
