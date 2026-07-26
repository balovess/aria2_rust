use std::sync::atomic::{AtomicUsize, Ordering};

use crate::selector::param_expander::expand_parameterized_uri;

pub trait UriSelector: Send + Sync {
    /// Select the next URI from the candidate list.
    ///
    /// Returns the index of the selected URI, or `None` if no URI is available.
    /// The caller is responsible for removing the URI from the source collection
    /// (e.g., `FileEntry::remaining_uris`) after a successful selection.
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

/// In-order URI selector that always picks the front (first) URI.
///
/// Mirrors the C++ `InorderURISelector::select()` which pops from the front
/// of `FileEntry::remainingUris_`. Since the caller removes the selected URI
/// from `remaining_uris` after each successful selection, always returning
/// index 0 naturally produces the "pick next untried URI in order" behavior.
///
/// No internal counter is needed — the ordering is implicit in the
/// `remaining_uris` deque maintained by `FileEntry`.
pub struct InorderUriSelector;

impl InorderUriSelector {
    pub fn new() -> Self {
        Self
    }
}

impl Default for InorderUriSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl UriSelector for InorderUriSelector {
    /// Always return index 0 (front of remaining URIs).
    ///
    /// Matches C++ `InorderURISelector::select()`:
    /// ```cpp
    /// std::string nextURI = uris.front();
    /// uris.pop_front();
    /// return nextURI;
    /// ```
    fn select(&self, uris: &[String], _used_hosts: &[(usize, String)]) -> Option<usize> {
        if uris.is_empty() {
            None
        } else {
            Some(0)
        }
    }

    fn tune_command(&self, _uris: &[String], _speed: u64) {}

    fn reset(&self) {
        // No-op: InorderUriSelector has no internal state to reset.
        // The ordering is determined by the `remaining_uris` deque in FileEntry.
    }
}

/// URI selector that sorts by explicit priority values
pub struct PriorityUriSelector {
    priorities: Vec<i32>,    // one per URI, lower number = higher preference (tried first)
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

        // Sort indices by priority ascending (lower number = higher preference)
        let mut indexed: Vec<(usize, i32)> = self.priorities.iter().cloned().enumerate().collect();
        indexed.sort_by_key(|b| b.1); // ascending: lowest priority number first

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

    /// Test that InorderUriSelector always returns index 0 (pop-from-front).
    ///
    /// In C++, `InorderURISelector::select()` pops from the front of
    /// `FileEntry::remainingUris_`. The caller then removes the URI
    /// from the deque, so the next call to `select()` also returns index 0
    /// (which is now the next URI). This test simulates that pattern.
    #[test]
    fn test_inorder_always_returns_front() {
        let selector = InorderUriSelector::new();

        // Simulate: remaining_uris = ["A", "B", "C"]
        let mut uris = vec![
            "http://a.com/1".to_string(),
            "http://b.com/2".to_string(),
            "http://c.com/3".to_string(),
        ];

        // First selection: returns 0 → "A", caller removes it
        let r0 = selector.select(&uris, &[]);
        assert_eq!(r0, Some(0));
        uris.remove(0);

        // Second selection: returns 0 → "B" (now at front), caller removes it
        let r1 = selector.select(&uris, &[]);
        assert_eq!(r1, Some(0));
        uris.remove(0);

        // Third selection: returns 0 → "C", caller removes it
        let r2 = selector.select(&uris, &[]);
        assert_eq!(r2, Some(0));
        uris.remove(0);

        // Empty: returns None
        assert!(selector.select(&uris, &[]).is_none());
    }

    /// Test that reset() is a no-op for InorderUriSelector (no internal state).
    #[test]
    fn test_reset_is_noop() {
        let selector = InorderUriSelector::new();
        let uris = vec!["http://a.com".to_string(), "http://b.com".to_string()];

        // reset() is a no-op, select() still returns 0
        selector.reset();
        assert_eq!(selector.select(&uris, &[]), Some(0));

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
        // Priority values: lower number = higher preference (tried first)
        let selector = PriorityUriSelector::new(vec![1, 3, 2]); // index 0 has lowest number = highest preference
        let uris = vec![
            "http://best.com/a".to_string(),
            "http://worst.com/b".to_string(),
            "http://medium.com/c".to_string(),
        ];

        let result = selector.select(&uris, &[]);
        assert_eq!(
            result,
            Some(0),
            "Should select lowest priority number URI (index 0, priority 1)"
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
        // Test that InorderUriSelector works correctly with prepared candidates.
        // The selector always returns 0; the caller removes from the front.
        let selector = InorderUriSelector::new();
        let candidates = vec!["http://example.com/file${01-02}.txt".to_string()];

        let mut expanded = prepare_candidates(&candidates);

        // Select from front, then remove (simulating C++ pop_front behavior)
        let r0 = selector.select(&expanded, &[]);
        assert_eq!(r0, Some(0));
        expanded.remove(0);

        let r1 = selector.select(&expanded, &[]);
        assert_eq!(r1, Some(0)); // Still 0 because we removed the previous front
        expanded.remove(0);

        // Empty now
        assert!(selector.select(&expanded, &[]).is_none());
    }
}
