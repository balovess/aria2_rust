use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub trait UriSelector: Send + Sync {
    fn select(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize>;

    fn tune_command(&self, _uris: &[String], _speed: u64) {}

    fn reset(&self);

    /// Report a failure for a specific URI index (default no-op)
    fn report_failure(&mut self, _uri_idx: usize) {}
}

pub struct InorderUriSelector {
    current_index: AtomicU32,
}

impl InorderUriSelector {
    pub fn new() -> Self {
        Self {
            current_index: AtomicU32::new(0),
        }
    }
}

impl Default for InorderUriSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl UriSelector for InorderUriSelector {
    fn select(&self, uris: &[String], _used_hosts: &[(usize, String)]) -> Option<usize> {
        if uris.is_empty() {
            return None;
        }
        let idx = self.current_index.fetch_add(1, Ordering::Relaxed) as usize % uris.len();
        Some(idx)
    }

    fn tune_command(&self, _uris: &[String], _speed: u64) {}

    fn reset(&self) {
        self.current_index.store(0, Ordering::Relaxed);
    }
}

/// URI selector that sorts by explicit priority values
pub struct PriorityUriSelector {
    priorities: Vec<i32>,    // one per URI, higher = tried first
    rr_counter: AtomicUsize, // round-robin counter for equal priority
}

impl PriorityUriSelector {
    pub fn new(priorities: Vec<i32>) -> Self {
        Self {
            priorities,
            rr_counter: AtomicUsize::new(0),
        }
    }
}

impl UriSelector for PriorityUriSelector {
    fn select(&self, uris: &[String], _used_hosts: &[(usize, String)]) -> Option<usize> {
        if uris.is_empty() || self.priorities.is_empty() {
            return None;
        }

        // Sort indices by priority descending
        let mut indexed: Vec<(usize, i32)> = self.priorities.iter().cloned().enumerate().collect();
        indexed.sort_by(|a, b| b.1.cmp(&a.1)); // descending

        // Among highest priority group, round-robin
        let top_priority = indexed.first().map(|&(_, p)| p)?;
        let top_group: Vec<usize> = indexed
            .into_iter()
            .filter(|&(_, p)| p == top_priority)
            .map(|(i, _)| i)
            .collect();

        if top_group.len() <= 1 {
            return Some(top_group[0]);
        }

        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) % top_group.len();
        Some(top_group[idx])
    }

    fn reset(&self) {
        self.rr_counter.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inorder_empty_uris() {
        let selector = InorderUriSelector::new();
        assert!(selector.select(&[], &[]).is_none());
    }

    #[test]
    fn test_inorder_single_uri() {
        let selector = InorderUriSelector::new();
        let uris = vec!["http://example.com/file".to_string()];
        assert_eq!(selector.select(&uris, &[]), Some(0));
    }

    #[test]
    fn test_inorder_cycling() {
        let selector = InorderUriSelector::new();
        let uris = vec![
            "http://a.com/1".to_string(),
            "http://b.com/2".to_string(),
            "http://c.com/3".to_string(),
        ];

        let r0 = selector.select(&uris, &[]);
        let r1 = selector.select(&uris, &[]);
        let r2 = selector.select(&uris, &[]);
        let r3 = selector.select(&uris, &[]);

        assert_eq!(r0, Some(0));
        assert_eq!(r1, Some(1));
        assert_eq!(r2, Some(2));
        assert_eq!(r3, Some(0)); // wraps around
    }

    #[test]
    fn test_reset() {
        let selector = InorderUriSelector::new();
        let uris = vec!["http://a.com".to_string(), "http://b.com".to_string()];

        selector.select(&uris, &[]);
        selector.select(&uris, &[]);
        selector.reset();

        assert_eq!(selector.select(&uris, &[]), Some(0));
    }

    #[test]
    fn test_tune_command_no_panic() {
        let selector = InorderUriSelector::new();
        let uris = vec!["http://example.com".to_string()];
        selector.tune_command(&uris, 9999);
    }

    #[test]
    fn test_used_hosts_ignored_by_inorder() {
        let selector = InorderUriSelector::new();
        let uris = vec!["http://a.com".to_string(), "http://b.com".to_string()];
        let used = vec![(0, "a.com".to_string())];
        let result = selector.select(&uris, &used);
        assert!(result.is_some());
    }

    // ======================================================================
    // PriorityUriSelector Tests
    // ======================================================================

    #[test]
    fn test_priority_selector_sorting() {
        let selector = PriorityUriSelector::new(vec![1, 3, 2]); // index 1 has highest priority
        let uris = vec![
            "http://low.com/a".to_string(),
            "http://high.com/b".to_string(),
            "http://medium.com/c".to_string(),
        ];

        let result = selector.select(&uris, &[]);
        assert_eq!(
            result,
            Some(1),
            "Should select highest priority URI (index 1)"
        );
    }

    #[test]
    fn test_priority_selector_round_robin() {
        let selector = PriorityUriSelector::new(vec![5, 5, 5]); // all same priority
        let uris = vec![
            "http://a.com/1".to_string(),
            "http://b.com/2".to_string(),
            "http://c.com/3".to_string(),
        ];

        let r0 = selector.select(&uris, &[]);
        let r1 = selector.select(&uris, &[]);
        let r2 = selector.select(&uris, &[]);
        let r3 = selector.select(&uris, &[]);

        // Should rotate through all three
        assert_eq!(r0, Some(0));
        assert_eq!(r1, Some(1));
        assert_eq!(r2, Some(2));
        assert_eq!(r3, Some(0)); // wraps around
    }

    #[test]
    fn test_priority_selector_empty() {
        let selector = PriorityUriSelector::new(vec![]);
        let uris = vec!["http://example.com".to_string()];
        assert!(selector.select(&uris, &[]).is_none());
    }

    #[test]
    fn test_priority_selector_single_uri() {
        let selector = PriorityUriSelector::new(vec![10]);
        let uris = vec!["http://only.com/file".to_string()];
        assert_eq!(selector.select(&uris, &[]), Some(0));
    }
}
