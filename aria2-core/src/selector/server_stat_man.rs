use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::selector::server_stat::ServerStat;

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
}
