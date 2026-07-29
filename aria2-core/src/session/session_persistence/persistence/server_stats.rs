//! Server statistics persistence for SessionPersistence.

use tracing::{debug, info};

use crate::selector::server_stat_man::ServerStatMan;

use super::types::SessionPersistence;

impl SessionPersistence {
    /// Save server statistics to the session directory.
    ///
    /// Persists all server performance statistics (download speeds, error counts,
    /// etc.) to a JSON file in the session directory. This allows the adaptive
    /// URI selector to remember server performance across restarts.
    ///
    /// # Arguments
    ///
    /// * `stat_man` - Reference to the ServerStatMan to save
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of server stats saved
    /// * `Err(String)` - Error message if save fails
    ///
    /// # File Location
    ///
    /// Stats are saved to `{session_dir}/server-stat.json`
    ///
    /// # Example
    ///
    /// ```ignore
    /// use aria2_core::session::session_persistence::SessionPersistence;
    /// use aria2_core::selector::server_stat_man::ServerStatMan;
    ///
    /// let persistence = SessionPersistence::new(Path::new("/tmp/aria2_session"));
    /// let stat_man = ServerStatMan::new();
    /// stat_man.update("fast.mirror.com", 10000, false);
    ///
    /// let saved = persistence.save_server_stats(&stat_man).await?;
    /// println!("Saved {} server stats", saved);
    /// ```
    pub async fn save_server_stats(&self, stat_man: &ServerStatMan) -> Result<usize, String> {
        let stat_file = self.session_dir.join("server-stat.json");
        let saved = stat_man.save_to_file_async(&stat_file).await?;

        if saved > 0 {
            debug!(
                count = saved,
                path = %stat_file.display(),
                "Server statistics saved"
            );
        }

        Ok(saved)
    }

    /// Load server statistics from the session directory.
    ///
    /// Restores previously saved server performance statistics from a JSON file
    /// in the session directory. This allows the adaptive URI selector to
    /// make informed decisions immediately after startup.
    ///
    /// # Arguments
    ///
    /// * `stat_man` - Reference to the ServerStatMan to load into
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of server stats loaded
    /// * `Err(String)` - Error message if load fails
    ///
    /// # Behavior
    ///
    /// - Returns `Ok(0)` if no server-stat.json file exists (not an error)
    /// - Returns error if file exists but is invalid
    /// - Merges with existing stats (doesn't clear current stats)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use aria2_core::session::session_persistence::SessionPersistence;
    /// use aria2_core::selector::server_stat_man::ServerStatMan;
    ///
    /// let persistence = SessionPersistence::new(Path::new("/tmp/aria2_session"));
    /// let stat_man = ServerStatMan::new();
    ///
    /// let loaded = persistence.load_server_stats(&stat_man).await?;
    /// println!("Loaded {} server stats from previous session", loaded);
    /// ```
    pub async fn load_server_stats(&self, stat_man: &ServerStatMan) -> Result<usize, String> {
        let stat_file = self.session_dir.join("server-stat.json");

        if !stat_file.exists() {
            debug!("No server statistics file found, starting fresh");
            return Ok(0);
        }

        let loaded = stat_man.load_from_file_async(&stat_file).await?;

        if loaded > 0 {
            info!(
                count = loaded,
                path = %stat_file.display(),
                "Server statistics loaded from previous session"
            );
        }

        Ok(loaded)
    }
}
