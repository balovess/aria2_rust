use std::sync::atomic::{AtomicU32, Ordering};

pub trait UriSelector: Send + Sync {
    fn select(
        &self,
        uris: &[String],
        used_hosts: &[(usize, String)],
    ) -> Option<usize>;

    fn tune_command(&self, _uris: &[String], _speed: u64) {}

    fn reset(&self);
}

pub struct InorderUriSelector {
    current_index: AtomicU32,
}

impl InorderUriSelector {
    pub fn new() -> Self {
        Self { current_index: AtomicU32::new(0) }
    }
}

impl Default for InorderUriSelector {
    fn default() -> Self { Self::new() }
}

impl UriSelector for InorderUriSelector {
    fn select(&self, uris: &[String], _used_hosts: &[(usize, String)]) -> Option<usize> {
        if uris.is_empty() { return None; }
        let idx = self.current_index.fetch_add(1, Ordering::Relaxed) as usize % uris.len();
        Some(idx)
    }

    fn tune_command(&self, _uris: &[String], _speed: u64) {}

    fn reset(&self) {
        self.current_index.store(0, Ordering::Relaxed);
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
        let uris = vec![
            "http://a.com".to_string(),
            "http://b.com".to_string(),
        ];
        let used = vec![(0, "a.com".to_string())];
        let result = selector.select(&uris, &used);
        assert!(result.is_some());
    }
}
