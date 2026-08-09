//! SessionPersistence struct definition, constants, constructors, and accessors.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::http::cookie_storage::{CookieJar, CookieStorage};

/// Default auto-save interval in seconds
pub const DEFAULT_AUTO_SAVE_INTERVAL_SECS: u64 = 60;

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
    /// Auto-save interval
    pub(crate) auto_save_interval: Duration,
    /// Whether auto-save is enabled
    pub(crate) auto_save_enabled: bool,
    /// Canonical shared storage persisted alongside session data.
    pub(crate) cookie_storage: Arc<CookieStorage>,
    /// Legacy cookie jar retained for JSON/API compatibility.
    pub(crate) cookie_jar: Option<CookieJar>,
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
            auto_save_interval: Duration::from_secs(DEFAULT_AUTO_SAVE_INTERVAL_SECS),
            auto_save_enabled: true,
            cookie_storage: CookieStorage::shared(),
            cookie_jar: None,
        }
    }

    /// Create with custom auto-save interval
    pub fn with_interval(mut self, interval_secs: u64) -> Self {
        self.auto_save_interval = Duration::from_secs(interval_secs.max(10));
        self
    }

    /// Disable auto-save (only manual save/load)
    pub fn without_auto_save(mut self) -> Self {
        self.auto_save_enabled = false;
        self
    }

    /// Bind canonical shared cookie storage for persistence.
    pub fn with_cookie_storage(mut self, storage: Arc<CookieStorage>) -> Self {
        self.cookie_storage = storage;
        self
    }

    /// Set cookie jar for persistence alongside session data.
    /// Retained for JSON/API compatibility; new code should use `with_cookie_storage`.
    pub fn with_cookie_jar(mut self, jar: CookieJar) -> Self {
        self.cookie_jar = Some(jar);
        self
    }

    /// Get mutable reference to the cookie jar for adding cookies before saving
    pub fn cookie_jar_mut(&mut self) -> Option<&mut CookieJar> {
        self.cookie_jar.as_mut()
    }

    /// Get reference to the cookie jar
    pub fn cookie_jar(&self) -> Option<&CookieJar> {
        self.cookie_jar.as_ref()
    }

    /// Get the session directory path
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }
}
