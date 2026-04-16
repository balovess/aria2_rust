use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::selector::param_expander::expand_parameterized_uri;

pub trait UriSelector: Send + Sync {
    fn select(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize>;

    fn tune_command(&self, _uris: &[String], _speed: u64) {}

    fn reset(&self);

    /// Report a failure for a specific URI index (default no-op)
    fn report_failure(&mut self, _uri_idx: usize) {}
}

/// Prepare candidate URIs by expanding any parameterized patterns.
///
/// This function checks each URI for parameterized patterns (like `$num`, `${...}`, `[...]`)
/// and expands them into concrete URIs before selection. This ensures that batch download
/// patterns are properly resolved before the selector makes its choice.
///
/// # Arguments
/// * `uris` - Slice of candidate URIs (may contain parameterized patterns)
///
/// # Returns
/// A new `Vec<String>` with all parameterized URIs expanded. Normal URIs are preserved
/// as-is. The order is maintained: expanded sequences appear in place of their source pattern.
///
/// # Example
/// ```ignore
/// let candidates = vec![
///     "http://example.com/file${01-03}.txt".to_string(),
///     "http://backup.com/file.txt".to_string(),
/// ];
/// let expanded = prepare_candidates(&candidates);
/// // Results in 4 URIs: file01.txt, file02.txt, file03.txt, plus backup
/// ```
pub fn prepare_candidates(uris: &[String]) -> Vec<String> {
    let mut result = Vec::new();

    for uri in uris {
        let expanded = expand_parameterized_uri(uri);
        result.extend(expanded);
    }

    result
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

    // ======================================================================
    // Tests for prepare_candidates (parameterized URI expansion)
    // ======================================================================

    #[test]
    fn test_prepare_candidates_basic_expansion() {
        let candidates = vec![
            "http://example.com/file${01-03}.txt".to_string(),
            "http://backup.com/static.txt".to_string(),
        ];

        let expanded = prepare_candidates(&candidates);

        assert_eq!(expanded.len(), 4);
        assert_eq!(expanded[0], "http://example.com/file01.txt");
        assert_eq!(expanded[1], "http://example.com/file02.txt");
        assert_eq!(expanded[2], "http://example.com/file03.txt");
        assert_eq!(expanded[3], "http://backup.com/static.txt");
    }

    #[test]
    fn test_prepare_candidates_mixed_patterns() {
        let candidates = vec![
            "http://a.com/${1-2}.dat".to_string(),
            "http://b.com/[01-02].zip".to_string(),
            "http://c.com/normal.html".to_string(),
        ];

        let expanded = prepare_candidates(&candidates);

        // 2 + 2 + 1 = 5 URIs
        assert_eq!(expanded.len(), 5);
        // First two from ${1-2}
        assert_eq!(expanded[0], "http://a.com/1.dat");
        assert_eq!(expanded[1], "http://a.com/2.dat");
        // Next two from [01-02]
        assert_eq!(expanded[2], "http://b.com/01.zip");
        assert_eq!(expanded[3], "http://b.com/02.zip");
        // Last one is normal
        assert_eq!(expanded[4], "http://c.com/normal.html");
    }

    #[test]
    fn test_prepare_candidates_all_normal() {
        let candidates = vec![
            "http://example.com/a.txt".to_string(),
            "http://example.com/b.txt".to_string(),
        ];

        let expanded = prepare_candidates(&candidates);

        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded, candidates);
    }

    #[test]
    fn test_prepare_candidates_empty_input() {
        let candidates: Vec<String> = vec![];
        let expanded = prepare_candidates(&candidates);

        assert!(expanded.is_empty());
    }

    #[test]
    fn test_prepare_candidates_cartesian_product() {
        let candidates = vec!["http://ex.com/${01-02}-${01-02}.html".to_string()];

        let expanded = prepare_candidates(&candidates);

        assert_eq!(expanded.len(), 4); // 2x2 Cartesian product
        assert_eq!(expanded[0], "http://ex.com/01-01.html");
        assert_eq!(expanded[3], "http://ex.com/02-02.html");
    }

    #[test]
    fn test_prepare_candidates_with_selector_integration() {
        // Test that InorderUriSelector works correctly with prepared candidates
        let selector = InorderUriSelector::new();
        let candidates = vec!["http://example.com/file${01-02}.txt".to_string()];

        let expanded = prepare_candidates(&candidates);

        // Select should work on the expanded list
        let r0 = selector.select(&expanded, &[]);
        let r1 = selector.select(&expanded, &[]);
        let r2 = selector.select(&expanded, &[]);

        assert_eq!(r0, Some(0));
        assert_eq!(r1, Some(1));
        assert_eq!(r2, Some(0)); // wraps around
    }
}
