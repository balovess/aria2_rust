//! Active output path registry — prevents silent filename collisions between concurrent downloads.
//!
//! When multiple `DownloadCommand` instances target the same output directory with the same
//! inferred filename, the last writer would silently overwrite previous results. This module
//! provides a process-wide registry that detects such collisions and automatically appends
//! a `(N)` suffix (Windows/Mac style) to produce unique filenames.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Process-wide registry of output paths that are currently being written by active downloads.
///
/// All download command types (`DownloadCommand`, `MetalinkDownloadCommand`,
/// `ConcurrentDownloadCommand`) consult this registry **before** opening their disk writer,
/// so that concurrent downloads targeting the same filename receive distinct paths.
pub struct ActiveOutputRegistry {
    inner: Arc<RwLock<HashSet<PathBuf>>>,
}

impl Default for ActiveOutputRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveOutputRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Resolve the final output path for a download, registering it to prevent collisions.
    ///
    /// If `desired` is not currently claimed by another active download, it is registered
    /// and returned as-is.  If a collision is detected, the method appends ` (1)`, ` (2)`,
    /// etc. (before the file extension) until an unused name is found.
    ///
    /// # Returns
    ///
    /// The resolved `PathBuf` that the caller should use for all subsequent disk I/O.
    /// The caller **must** call [`Self::release`] when the download finishes (success or failure).
    pub async fn resolve(&self, desired: &Path) -> PathBuf {
        let mut registry = self.inner.write().await;

        if !registry.contains(desired) {
            registry.insert(desired.to_path_buf());
            debug!(
                "Output path registered (no conflict): {}",
                desired.display()
            );
            return desired.to_path_buf();
        }

        // Collision detected — generate unique name with numeric suffix.
        let stem = desired
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        let ext = desired
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();

        let parent = desired.parent().unwrap_or_else(|| Path::new("."));

        let mut counter: u32 = 1;
        loop {
            let candidate = parent.join(format!("{} ({}){}", stem, counter, ext));

            // Check both the in-progress registry AND the filesystem to avoid conflicts
            // with previously completed downloads that happened to use the same suffix.
            if !registry.contains(&candidate) && !candidate.exists() {
                registry.insert(candidate.clone());
                warn!(
                    "Filename collision: '{}' is already in use. Resolved to '{}'",
                    desired.display(),
                    candidate.display()
                );
                return candidate;
            }

            counter += 1;

            // Safety upper bound to prevent unbounded looping in pathological cases.
            if counter > 10_000 {
                let fallback = parent.join(format!("{}_collision_{}", stem, counter));
                registry.insert(fallback.clone());
                warn!(
                    "Exhausted normal suffix range for '{}', using fallback '{}'",
                    desired.display(),
                    fallback.display()
                );
                return fallback;
            }
        }
    }

    /// Release a previously-resolved path from the registry.
    ///
    /// Must be called when a download completes (success or failure) so that the path
    /// becomes available for future downloads if needed.
    pub async fn release(&self, path: &Path) {
        let mut registry = self.inner.write().await;
        if registry.remove(path) {
            debug!("Output path released: {}", path.display());
        }
    }

    /// Return the number of paths currently registered (for diagnostics / testing).
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Check whether the registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Global singleton — every download command shares the same instance so that
// cross-task collisions are caught regardless of how commands are spawned.
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

/// Global singleton registry instance. Initialized on first access.
static GLOBAL_REGISTRY: OnceLock<ActiveOutputRegistry> = OnceLock::new();

/// Obtain a reference to the global `ActiveOutputRegistry`.
pub fn global_registry() -> &'static ActiveOutputRegistry {
    GLOBAL_REGISTRY.get_or_init(|| {
        info!("Global ActiveOutputRegistry initialized");
        ActiveOutputRegistry::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_no_collision_when_unique() {
        let reg = ActiveOutputRegistry::new();
        let p1 = reg.resolve(Path::new("/tmp/file.txt")).await;
        assert_eq!(p1, PathBuf::from("/tmp/file.txt"));
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn test_collision_generates_suffix() {
        let reg = ActiveOutputRegistry::new();

        let p1 = reg.resolve(Path::new("/tmp/file.txt")).await;
        let p2 = reg.resolve(Path::new("/tmp/file.txt")).await;

        assert_eq!(p1, PathBuf::from("/tmp/file.txt"));
        assert_eq!(p2, PathBuf::from("/tmp/file (1).txt"));
        assert_eq!(reg.len().await, 2);
    }

    #[tokio::test]
    async fn test_triple_collision() {
        let reg = ActiveOutputRegistry::new();

        let _p1 = reg.resolve(Path::new("/tmp/data.bin")).await;
        let _p2 = reg.resolve(Path::new("/tmp/data.bin")).await;
        let p3 = reg.resolve(Path::new("/tmp/data.bin")).await;

        assert_eq!(p3, PathBuf::from("/tmp/data (2).bin"));
    }

    #[tokio::test]
    async fn test_release_allows_reuse() {
        let reg = ActiveOutputRegistry::new();

        let p1 = reg.resolve(Path::new("/tmp/reusable.txt")).await;
        reg.release(&p1).await;
        assert!(reg.is_empty().await);

        // After release, the same path can be claimed again.
        let p2 = reg.resolve(Path::new("/tmp/reusable.txt")).await;
        assert_eq!(p2, PathBuf::from("/tmp/reusable.txt"));
    }

    #[tokio::test]
    async fn test_no_extension_handling() {
        let reg = ActiveOutputRegistry::new();

        let p1 = reg.resolve(Path::new("/tmp/Makefile")).await;
        let p2 = reg.resolve(Path::new("/tmp/Makefile")).await;

        assert_eq!(p1, PathBuf::from("/tmp/Makefile"));
        // Files without extension should still get suffixed correctly.
        assert_eq!(p2, PathBuf::from("/tmp/Makefile (1)"));
    }

    #[tokio::test]
    async fn test_global_singleton() {
        let r1 = global_registry();
        let r2 = global_registry();
        // Both references must point to the same underlying registry.
        assert!(Arc::ptr_eq(&r1.inner, &r2.inner));
    }
}
