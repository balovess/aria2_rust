use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::selector::server_stat::{ServerStat, ServerStatSnapshot};

/// Container for serialized server statistics file.
///
/// This is the top-level structure saved to disk when persisting
/// server statistics. It includes metadata about when the file was
/// saved and the version format.
///
/// # Example JSON structure
///
/// ```json
/// {
///   "version": "1.0",
///   "saved_at": 1700000000,
///   "servers": [
///     {
///       "host": "mirror.example.com",
///       "download_speed": 5000000,
///       ...
///     }
///   ]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatFile {
    /// File format version (currently "1.0")
    pub version: String,
    /// Unix timestamp when the file was saved
    pub saved_at: u64,
    /// List of server statistics
    pub servers: Vec<ServerStatSnapshot>,
}

pub struct ServerStatMan {
    stats: RwLock<HashMap<String, Arc<ServerStat>>>,
}

impl ServerStatMan {
    pub fn new() -> Self {
        Self {
            stats: RwLock::new(HashMap::new()),
        }
    }

    pub fn get_or_create(&self, host: &str) -> Arc<ServerStat> {
        let mut map = self.stats.write().unwrap();
        if let Some(stat) = map.get(host) {
            Arc::clone(stat)
        } else {
            let stat = Arc::new(ServerStat::new(host));
            map.insert(host.to_string(), Arc::clone(&stat));
            stat
        }
    }

    pub fn find_stat(&self, host: &str) -> Option<Arc<ServerStat>> {
        let map = self.stats.read().unwrap();
        map.get(host).cloned()
    }

    pub fn update(&self, host: &str, dl_speed: u64, is_multi: bool) {
        let stat = self.get_or_create(host);
        stat.update_speed(dl_speed, is_multi);
    }

    pub fn get_all_stats(&self) -> Vec<Arc<ServerStat>> {
        let map = self.stats.read().unwrap();
        map.values().cloned().collect()
    }

    pub fn remove(&self, host: &str) {
        let mut map = self.stats.write().unwrap();
        map.remove(host);
    }

    pub fn count(&self) -> usize {
        let map = self.stats.read().unwrap();
        map.len()
    }

    pub fn hosts(&self) -> Vec<String> {
        let map = self.stats.read().unwrap();
        map.keys().cloned().collect()
    }

    /// Mark a host as failed, updating error tracking fields.
    ///
    /// Clones the existing ServerStat, applies failure info via set_failure_info,
    /// and replaces the entry in the map so all future Arc holders see the update.
    pub fn mark_failure(&self, host: &str, error_code: u16) {
        let mut map = self.stats.write().unwrap();
        if let Some(stat_arc) = map.get(host) {
            // Dereference Arc to get inner ServerStat, then clone the inner value
            let inner: &ServerStat = stat_arc;
            let mut updated = inner.clone();
            updated.set_failure_info(error_code);
            map.insert(host.to_string(), Arc::new(updated));
        }
    }

    // ==================== Persistence Methods ====================

    /// Save all server statistics to a JSON file (synchronous version).
    ///
    /// Serializes all server statistics to a JSON file at the specified path.
    /// The file format includes version info, timestamp, and all server stats.
    ///
    /// # Arguments
    ///
    /// * `path` - File path to save to (e.g., "server-stat.json")
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of servers saved
    /// * `Err(String)` - Error message if save fails
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aria2_core::selector::server_stat_man::ServerStatMan;
    /// use std::path::Path;
    ///
    /// let man = ServerStatMan::new();
    /// man.update("mirror.example.com", 5000, false);
    ///
    /// let saved = man.save_to_file(Path::new("server-stat.json")).unwrap();
    /// println!("Saved {} servers", saved);
    /// ```
    pub fn save_to_file(&self, path: &Path) -> Result<usize, String> {
        let map = self.stats.read().unwrap();

        let servers: Vec<ServerStatSnapshot> = map.values().map(|stat| stat.to_snapshot()).collect();

        let file_content = ServerStatFile {
            version: "1.0".to_string(),
            saved_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            servers,
        };

        let json = serde_json::to_string_pretty(&file_content)
            .map_err(|e| format!("Failed to serialize server stats: {}", e))?;

        std::fs::write(path, json)
            .map_err(|e| format!("Failed to write server stat file: {}", e))?;

        Ok(file_content.servers.len())
    }

    /// Load server statistics from a JSON file (synchronous version).
    ///
    /// Reads a JSON file and restores all server statistics into memory.
    /// Existing statistics are preserved; loaded stats are merged in.
    ///
    /// # Arguments
    ///
    /// * `path` - File path to load from
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of servers loaded
    /// * `Err(String)` - Error message if load fails
    ///
    /// # Behavior
    ///
    /// - Returns `Ok(0)` if the file doesn't exist (not an error)
    /// - Returns error if file exists but is invalid JSON
    /// - Merges with existing stats (doesn't clear current stats)
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aria2_core::selector::server_stat_man::ServerStatMan;
    /// use std::path::Path;
    ///
    /// let man = ServerStatMan::new();
    /// let loaded = man.load_from_file(Path::new("server-stat.json")).unwrap();
    /// println!("Loaded {} servers", loaded);
    /// ```
    pub fn load_from_file(&self, path: &Path) -> Result<usize, String> {
        if !path.exists() {
            return Ok(0); // Not an error if file doesn't exist
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read server stat file: {}", e))?;

        let file_content: ServerStatFile = serde_json::from_str(&content)
            .map_err(|e| format!("Invalid server stat file format: {}", e))?;

        let count = file_content.servers.len();
        let mut map = self.stats.write().unwrap();

        for snapshot in file_content.servers {
            let stat = Arc::new(ServerStat::from_snapshot(&snapshot));
            map.insert(snapshot.host, stat);
        }

        Ok(count)
    }

    /// Save all server statistics to a JSON file (async version).
    ///
    /// Async variant of `save_to_file()` for use in async contexts.
    /// This is the preferred method when called from async code.
    ///
    /// # Arguments
    ///
    /// * `path` - File path to save to
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of servers saved
    /// * `Err(String)` - Error message if save fails
    pub async fn save_to_file_async(&self, path: &Path) -> Result<usize, String> {
        let path = path.to_path_buf();
        let snapshots: Vec<ServerStatSnapshot> = {
            let map = self.stats.read().unwrap();
            map.values().map(|stat| stat.to_snapshot()).collect()
        };

        let file_content = ServerStatFile {
            version: "1.0".to_string(),
            saved_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            servers: snapshots,
        };

        let json = serde_json::to_string_pretty(&file_content)
            .map_err(|e| format!("Failed to serialize server stats: {}", e))?;

        tokio::fs::write(&path, json)
            .await
            .map_err(|e| format!("Failed to write server stat file: {}", e))?;

        Ok(file_content.servers.len())
    }

    /// Load server statistics from a JSON file (async version).
    ///
    /// Async variant of `load_from_file()` for use in async contexts.
    /// This is the preferred method when called from async code.
    ///
    /// # Arguments
    ///
    /// * `path` - File path to load from
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of servers loaded
    /// * `Err(String)` - Error message if load fails
    pub async fn load_from_file_async(&self, path: &Path) -> Result<usize, String> {
        if !path.exists() {
            return Ok(0);
        }

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("Failed to read server stat file: {}", e))?;

        let file_content: ServerStatFile = serde_json::from_str(&content)
            .map_err(|e| format!("Invalid server stat file format: {}", e))?;

        let count = file_content.servers.len();
        let mut map = self.stats.write().unwrap();
        for snapshot in file_content.servers {
            let stat = Arc::new(ServerStat::from_snapshot(&snapshot));
            map.insert(snapshot.host, stat);
        }

        Ok(count)
    }
}

impl Default for ServerStatMan {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_creation_and_count() {
        let man = ServerStatMan::new();
        assert_eq!(man.count(), 0);
    }

    #[test]
    fn test_get_or_create_new_host() {
        let man = ServerStatMan::new();
        let stat = man.get_or_create("example.com");
        assert_eq!(stat.host, "example.com");
        assert_eq!(man.count(), 1);
    }

    #[test]
    fn test_get_or_create_returns_same_instance() {
        let man = ServerStatMan::new();
        let s1 = man.get_or_create("example.com");
        let s2 = man.get_or_create("example.com");
        assert!(Arc::ptr_eq(&s1, &s2));
        assert_eq!(man.count(), 1);
    }

    #[test]
    fn test_find_existing() {
        let man = ServerStatMan::new();
        man.get_or_create("example.com");
        assert!(man.find_stat("example.com").is_some());
        assert!(man.find_stat("nonexistent").is_none());
    }

    #[test]
    fn test_update_creates_if_needed() {
        let man = ServerStatMan::new();
        man.update("fast.server", 5000, false);
        assert_eq!(man.count(), 1);

        let stat = man.find_stat("fast.server").unwrap();
        assert_eq!(stat.get_download_speed(), 5000);
    }

    #[test]
    fn test_remove() {
        let man = ServerStatMan::new();
        man.get_or_create("a.com");
        man.get_or_create("b.com");
        assert_eq!(man.count(), 2);

        man.remove("a.com");
        assert_eq!(man.count(), 1);
        assert!(man.find_stat("a.com").is_none());
    }

    #[test]
    fn test_multiple_hosts_independent() {
        let man = ServerStatMan::new();
        man.update("fast.com", 10000, true);
        man.update("slow.com", 100, false);

        let fast = man.find_stat("fast.com").unwrap();
        let slow = man.find_stat("slow.com").unwrap();

        assert_ne!(fast.get_avg_speed(), slow.get_avg_speed());
        assert!(fast.get_avg_speed() > slow.get_avg_speed());
    }

    #[test]
    fn test_hosts_list() {
        let man = ServerStatMan::new();
        man.get_or_create("alpha.com");
        man.get_or_create("beta.com");
        let hosts = man.hosts();
        assert_eq!(hosts.len(), 2);
        assert!(hosts.contains(&"alpha.com".to_string()));
        assert!(hosts.contains(&"beta.com".to_string()));
    }

    // ======================================================================
    // Tests for mark_failure
    // ======================================================================

    #[test]
    fn test_mark_failure_updates_stats() {
        let man = ServerStatMan::new();
        man.get_or_create("failing.host");

        // Mark as failed with error code 500
        man.mark_failure("failing.host", 500);

        let stat = man.find_stat("failing.host").unwrap();
        assert_eq!(stat.get_consecutive_failures(), 1);
        assert!(stat.get_last_error_time() > 0);
        assert_eq!(stat.get_last_error_code(), 500);
    }

    #[test]
    fn test_mark_failure_multiple_times() {
        let man = ServerStatMan::new();
        man.get_or_create("repeated.failures");

        for i in 0..5u16 {
            man.mark_failure("repeated.failures", i);
        }

        let stat = man.find_stat("repeated.failures").unwrap();
        assert_eq!(stat.get_consecutive_failures(), 5);
        assert!(
            !stat.is_available(),
            "Should be unavailable after 5 failures"
        );
    }

    #[test]
    fn test_mark_failure_nonexistent_host() {
        let man = ServerStatMan::new();
        // Should not panic on nonexistent host
        man.mark_failure("nonexistent.host", 404);
        assert_eq!(man.count(), 0); // No stat created
    }

    // ======================================================================
    // Tests for Persistence (save/load)
    // ======================================================================

    #[test]
    fn test_save_to_file_basic() {
        let man = ServerStatMan::new();
        man.update("fast.mirror.com", 10000, false);
        man.update("slow.mirror.com", 100, true);

        let temp_file = std::env::temp_dir().join("test_server_stat_save.json");
        let saved = man.save_to_file(&temp_file).expect("Save should succeed");

        assert_eq!(saved, 2, "Should save 2 servers");

        // Verify file content is valid JSON
        let content = std::fs::read_to_string(&temp_file).expect("Should read file");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("Should be valid JSON");
        assert!(parsed.get("version").is_some());
        assert!(parsed.get("saved_at").is_some());
        assert!(parsed.get("servers").is_some());

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_load_from_file_basic() {
        let man = ServerStatMan::new();
        man.update("load.test.com", 5000, false);
        man.mark_failure("load.test.com", 500);

        let temp_file = std::env::temp_dir().join("test_server_stat_load.json");
        let _ = man.save_to_file(&temp_file);

        // Load into a new manager
        let man2 = ServerStatMan::new();
        let loaded = man2.load_from_file(&temp_file).expect("Load should succeed");

        assert_eq!(loaded, 1, "Should load 1 server");

        let stat = man2.find_stat("load.test.com").expect("Should find loaded server");
        assert!(stat.get_avg_speed() > 0);
        assert_eq!(stat.get_consecutive_failures(), 1);

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_save_load_roundtrip() {
        let man = ServerStatMan::new();
        man.update("mirror1.com", 10000, false);
        man.update("mirror2.com", 5000, true);
        man.update("mirror3.com", 2000, false);
        man.mark_failure("mirror3.com", 503);
        man.mark_failure("mirror3.com", 503);

        let temp_file = std::env::temp_dir().join("test_server_stat_roundtrip.json");
        let saved = man.save_to_file(&temp_file).unwrap();
        assert_eq!(saved, 3);

        let man2 = ServerStatMan::new();
        let loaded = man2.load_from_file(&temp_file).unwrap();
        assert_eq!(loaded, 3);

        // Verify all servers restored correctly
        let s1 = man2.find_stat("mirror1.com").unwrap();
        let s2 = man2.find_stat("mirror2.com").unwrap();
        let s3 = man2.find_stat("mirror3.com").unwrap();

        assert!(s1.get_single_avg_speed() > 0);
        assert!(s2.get_multi_avg_speed() > 0);
        assert_eq!(s3.get_consecutive_failures(), 2);
        assert_eq!(s3.get_last_error_code(), 503);

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_load_nonexistent_file() {
        let man = ServerStatMan::new();
        let nonexistent = std::path::Path::new("/tmp/nonexistent_server_stat_12345.json");

        let loaded = man.load_from_file(nonexistent).expect("Should return Ok(0) for nonexistent file");
        assert_eq!(loaded, 0);
        assert_eq!(man.count(), 0);
    }

    #[test]
    fn test_save_empty_manager() {
        let man = ServerStatMan::new();
        let temp_file = std::env::temp_dir().join("test_server_stat_empty.json");

        let saved = man.save_to_file(&temp_file).expect("Should save empty manager");
        assert_eq!(saved, 0);

        // Verify file is still valid JSON
        let content = std::fs::read_to_string(&temp_file).expect("Should read file");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("Should be valid JSON");
        let servers = parsed.get("servers").unwrap().as_array().unwrap();
        assert!(servers.is_empty());

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_load_merges_with_existing() {
        let man = ServerStatMan::new();
        man.update("existing.com", 1000, false);

        // Create a file with a different server
        let temp_file = std::env::temp_dir().join("test_server_stat_merge.json");
        let man2 = ServerStatMan::new();
        man2.update("fromfile.com", 5000, false);
        let _ = man2.save_to_file(&temp_file);

        // Load into man (which already has existing.com)
        let loaded = man.load_from_file(&temp_file).unwrap();
        assert_eq!(loaded, 1);

        // Should have both servers now
        assert_eq!(man.count(), 2);
        assert!(man.find_stat("existing.com").is_some());
        assert!(man.find_stat("fromfile.com").is_some());

        let _ = std::fs::remove_file(&temp_file);
    }

    #[tokio::test]
    async fn test_async_save_and_load() {
        let man = ServerStatMan::new();
        man.update("async.test.com", 8000, false);
        man.update("async2.test.com", 4000, true);

        let temp_file = std::env::temp_dir().join("test_server_stat_async.json");

        // Async save
        let saved = man
            .save_to_file_async(&temp_file)
            .await
            .expect("Async save should succeed");
        assert_eq!(saved, 2);

        // Async load
        let man2 = ServerStatMan::new();
        let loaded = man2
            .load_from_file_async(&temp_file)
            .await
            .expect("Async load should succeed");
        assert_eq!(loaded, 2);

        let stat = man2.find_stat("async.test.com").unwrap();
        assert!(stat.get_avg_speed() > 0);

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_file_format_structure() {
        let man = ServerStatMan::new();
        man.update("format.test", 12345, false);

        let temp_file = std::env::temp_dir().join("test_server_stat_format.json");
        let _ = man.save_to_file(&temp_file);

        let content = std::fs::read_to_string(&temp_file).unwrap();
        let parsed: ServerStatFile = serde_json::from_str(&content).unwrap();

        assert_eq!(parsed.version, "1.0");
        assert!(parsed.saved_at > 0);
        assert_eq!(parsed.servers.len(), 1);
        assert_eq!(parsed.servers[0].host, "format.test");
        assert_eq!(parsed.servers[0].download_speed, 12345);

        let _ = std::fs::remove_file(&temp_file);
    }
}
