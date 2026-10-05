//! SessionPersistence struct definition, constructors, and accessors.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::http::cookie::CookieStorage;

/// High-level session persistence manager
///
/// Coordinates saving and loading of download session state using the
/// ResumeData JSON format. Manages both individual command states (.aria2
/// files) and global session options.
///
/// # Examples
///
/// ```ignore
/// use aria2_core::session::session_persistence::SessionPersistence;
/// use std::path::Path;
///
/// let session = SessionPersistence::new(Path::new("/tmp/aria2_session"));
///
/// // Save current state
/// let count = session.save_state(&groups).await?;
/// println!("Saved {} downloads", count);
///
/// // Load saved state
/// let count = session.load_state(&mut groups).await?;
/// println!("Restored {} downloads", count);
/// ```
pub struct SessionPersistence {
    /// Directory where .aria2 files are stored
    pub(crate) session_dir: PathBuf,
    /// Canonical shared storage persisted alongside session data.
    pub(crate) cookie_storage: Arc<CookieStorage>,
}

impl SessionPersistence {
    /// Create a new SessionPersistence instance
    ///
    /// # Arguments
    ///
    /// * `session_dir` - Directory path for storing .aria2 session files
    pub fn new(session_dir: &Path) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            cookie_storage: CookieStorage::shared(),
        }
    }

    /// Bind canonical shared cookie storage for persistence.
    pub fn with_cookie_storage(mut self, storage: Arc<CookieStorage>) -> Self {
        self.cookie_storage = storage;
        self
    }

    /// Get the session directory path
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }
}
