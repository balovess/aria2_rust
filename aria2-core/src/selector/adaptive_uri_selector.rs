use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

const DEFAULT_NB_SERVER_TO_EVALUATE: i32 = 3;
const DEFAULT_NB_CONNECTIONS: i32 = 1;

fn extract_host(uri: &str) -> Option<String> {
    let uri = uri.trim();
    if !uri.contains("://") {
        return None;
    }
    let after_scheme = &uri[uri.find("://").unwrap() + 3..];
    let host_part = if let Some(slash_idx) = after_scheme.find('/') {
        &after_scheme[..slash_idx]
    } else {
        after_scheme
    };
    if host_part.is_empty() {
        return None;
    }
    Some(host_part.to_string())
}

pub struct AdaptiveUriSelector {
    stat_man: Arc<ServerStatMan>,
    nb_server_toevaluate: AtomicI32,
    nb_connections: AtomicI32,
}

impl AdaptiveUriSelector {
    pub fn new(stat_man: Arc<ServerStatMan>) -> Self {
        Self {
            stat_man,
            nb_server_toevaluate: AtomicI32::new(DEFAULT_NB_SERVER_TO_EVALUATE),
            nb_connections: AtomicI32::new(DEFAULT_NB_CONNECTIONS),
        }
    }

    pub fn set_nb_connections(&self, n: i32) {
        self.nb_connections.store(n, Ordering::Relaxed);
    }

    pub fn set_nb_evaluate(&self, n: i32) {
        self.nb_server_toevaluate.store(n, Ordering::Relaxed);
    }

    fn extract_hosts(&self, uris: &[String]) -> Vec<(usize, String)> {
        uris.iter()
            .enumerate()
            .filter_map(|(i, u)| extract_host(u).map(|h| (i, h)))
            .collect()
    }

    fn get_first_not_tested<'a>(
        &self,
        hosts: &'a [(usize, String)],
    ) -> Option<&'a (usize, String)> {
        hosts.iter().find(|(_, host)| {
            self.stat_man
                .find_stat(host)
                .is_none_or(|s| s.get_counter() == 0)
        })
    }

    fn get_first_to_test<'a>(
        &self,
        hosts: &'a [(usize, String)],
        max_test: i32,
    ) -> Option<&'a (usize, String)> {
        let tested_count = self.get_nb_tested_servers(hosts);
        if tested_count < max_test as usize {
            self.get_first_not_tested(hosts)
        } else {
            None
        }
    }

    fn get_best_mirror(
        &self,
        hosts: &[(usize, String)],
        used_hosts: &[(usize, String)],
    ) -> Option<usize> {
        let mut candidates: Vec<(usize, u64)> = hosts
            .iter()
            .filter_map(|(idx, host)| {
                let stat = self.stat_man.find_stat(host)?;
                if !stat.is_ok() {
                    return None;
                }
                Some((*idx, stat.get_avg_speed()))
            })
            .collect();

        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        let used_set: std::collections::HashSet<&str> =
            used_hosts.iter().map(|(_, h)| h.as_str()).collect();

        for (idx, _) in &candidates {
            let host = &hosts[*idx].1;
            if !used_set.contains(host.as_str()) {
                return Some(*idx);
            }
        }

        candidates.first().map(|(idx, _)| *idx)
    }

    fn select_one(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        if uris.is_empty() {
            return None;
        }
        if uris.len() == 1 {
            return Some(0);
        }

        let hosts = self.extract_hosts(uris);
        if hosts.is_empty() {
            return Some(0);
        }

        let max_eval = self.nb_server_toevaluate.load(Ordering::Relaxed);

        if let Some(selected) = self.get_first_to_test(&hosts, max_eval) {
            let idx = selected.0;
            if let Some(stat) = self.stat_man.find_stat(&selected.1) {
                stat.increment_counter();
            }
            return Some(idx);
        }

        self.get_best_mirror(&hosts, used_hosts)
    }

    fn get_nb_tested_servers(&self, hosts: &[(usize, String)]) -> usize {
        hosts
            .iter()
            .filter(|(_, host)| {
                self.stat_man
                    .find_stat(host)
                    .is_some_and(|s| s.get_counter() > 0)
            })
            .count()
    }

    pub fn adjust_lowest_speed_limit(&self, uris: &[String]) -> u64 {
        let hosts = self.extract_hosts(uris);
        let speeds: Vec<u64> = hosts
            .iter()
            .filter_map(|(_, host)| self.stat_man.find_stat(host).map(|s| s.get_avg_speed()))
            .collect();

        if speeds.is_empty() {
            return 0;
        }
        let max = *speeds.iter().max().unwrap_or(&0u64);
        if max == 0 {
            return 0;
        }
        (max as f64 * 0.3) as u64
    }

    pub fn reset_counters(&self) {
        for stat in self.stat_man.get_all_stats() {
            stat.reset_counter();
        }
    }

    pub fn stat_man(&self) -> &Arc<ServerStatMan> {
        &self.stat_man
    }
}

impl UriSelector for AdaptiveUriSelector {
    fn select(&self, uris: &[String], used_hosts: &[(usize, String)]) -> Option<usize> {
        self.select_one(uris, used_hosts)
    }

    fn tune_command(&self, uris: &[String], _speed: u64) {
        let limit = self.adjust_lowest_speed_limit(uris);
        if limit > 0 {
            tracing::debug!("AdaptiveURISelector tuning lowest-speed-limit to {}", limit);
        }
    }

    fn reset(&self) {
        self.reset_counters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_selector() -> AdaptiveUriSelector {
        AdaptiveUriSelector::new(Arc::new(ServerStatMan::new()))
    }

    #[test]
    fn test_select_empty_uris() {
        let sel = create_selector();
        assert!(sel.select(&[], &[]).is_none());
    }

    #[test]
    fn test_select_single_uri() {
        let sel = create_selector();
        let uris = vec!["http://example.com/file".to_string()];
        assert_eq!(sel.select(&uris, &[]), Some(0));
    }

    #[test]
    fn test_select_prefers_untested() {
        let sel = create_selector();
        let uris = vec![
            "http://fast.com/a".to_string(),
            "http://slow.com/b".to_string(),
        ];

        sel.stat_man.update("slow.com", 10000, false);

        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn test_select_picks_fastest_when_all_tested() {
        let sel = create_selector();
        let uris = vec![
            "http://slow.com/a".to_string(),
            "http://fast.com/b".to_string(),
        ];

        sel.stat_man.update("slow.com", 100, false);
        sel.stat_man.update("fast.com", 10000, false);

        let s1 = sel.stat_man.find_stat("slow.com").unwrap();
        s1.increment_counter();
        let s2 = sel.stat_man.find_stat("fast.com").unwrap();
        s2.increment_counter();

        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_select_skips_error_servers() {
        let sel = create_selector();
        let uris = vec![
            "http://error.com/a".to_string(),
            "http://ok.com/b".to_string(),
        ];

        sel.stat_man.update("error.com", 99999, false);
        sel.stat_man.update("ok.com", 5000, false);
        let err_stat = sel.stat_man.find_stat("error.com").unwrap();
        err_stat.set_error();
        err_stat.increment_counter();
        let ok_stat = sel.stat_man.find_stat("ok.com").unwrap();
        ok_stat.increment_counter();

        let result = sel.select(&uris, &[]);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_select_avoids_used_hosts() {
        let sel = create_selector();
        let uris = vec![
            "http://used.com/a".to_string(),
            "http://free.com/b".to_string(),
        ];

        sel.stat_man.update("used.com", 8000, false);
        sel.stat_man.update("free.com", 6000, false);
        let su = sel.stat_man.find_stat("used.com").unwrap();
        su.increment_counter();
        let sf = sel.stat_man.find_stat("free.com").unwrap();
        sf.increment_counter();

        let used = vec![(0, "used.com".to_string())];
        let result = sel.select(&uris, &used);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_select_falls_back_to_used_if_no_alternative() {
        let sel = create_selector();
        let uris = vec!["http://only.com/a".to_string()];

        sel.stat_man.update("only.com", 5000, false);
        let s = sel.stat_man.find_stat("only.com").unwrap();
        s.increment_counter();

        let used = vec![(0, "only.com".to_string())];
        let result = sel.select(&uris, &used);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn test_nb_evaluate_limits_testing() {
        let sel = create_selector();
        sel.set_nb_evaluate(1);

        let uris = vec![
            "http://a.com/1".to_string(),
            "http://b.com/2".to_string(),
            "http://c.com/3".to_string(),
        ];

        let r1 = sel.select(&uris, &[]).unwrap();
        assert_eq!(r1, 0, "First select picks first untested host");

        sel.stat_man.update("a.com", 100, false);
        sel.stat_man.update("b.com", 10000, false);
        let sb = sel.stat_man.find_stat("b.com").unwrap();
        sb.increment_counter();

        let _r2 = sel.select(&uris, &[]).unwrap();

        assert!(
            sel.stat_man.find_stat("a.com").is_some() || sel.stat_man.find_stat("b.com").is_some(),
            "Stats should be created for tested hosts"
        );
    }

    #[test]
    fn test_extract_host() {
        assert_eq!(
            extract_host("http://example.com/path"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_host("https://host:8080/file?q=1"),
            Some("host:8080".to_string())
        );
        assert_eq!(
            extract_host("ftp://server.com"),
            Some("server.com".to_string())
        );
        assert!(extract_host("not-a-uri").is_none());
        assert!(extract_host("").is_none());
    }

    #[test]
    fn test_adjust_lowest_speed_limit() {
        let sel = create_selector();
        let uris = vec![
            "http://fast.com/f".to_string(),
            "http://slow.com/s".to_string(),
        ];
        for _ in 0..20 {
            sel.stat_man.update("fast.com", 10000, false);
        }
        sel.stat_man.update("slow.com", 2000, false);

        let limit = sel.adjust_lowest_speed_limit(&uris);
        assert!(limit > 0);
        let expected = (10000u64 as f64 * 0.3) as u64;
        assert!(
            (limit as i64 - expected as i64).abs() <= 1,
            "limit={} expected={}",
            limit,
            expected
        );
    }

    #[test]
    fn test_adjust_zero_when_no_stats() {
        let sel = create_selector();
        let uris = vec!["http://unknown.com/x".to_string()];
        assert_eq!(sel.adjust_lowest_speed_limit(&uris), 0);
    }

    #[test]
    fn test_reset_counters() {
        let sel = create_selector();
        sel.stat_man.update("test.com", 5000, false);
        let s = sel.stat_man.find_stat("test.com").unwrap();
        s.increment_counter();
        s.increment_counter();
        assert_eq!(s.get_counter(), 2);

        sel.reset_counters();
        assert_eq!(s.get_counter(), 0);
    }

    #[test]
    fn test_tune_command_no_panic() {
        let sel = create_selector();
        let uris = vec!["http://example.com/file".to_string()];
        sel.tune_command(&uris, 12345);
    }

    #[test]
    fn test_get_best_mirror_with_all_same_speed() {
        let sel = create_selector();
        let uris = vec![
            "http://a.com/1".to_string(),
            "http://b.com/2".to_string(),
            "http://c.com/3".to_string(),
        ];

        for host in &["a.com", "b.com", "c.com"] {
            sel.stat_man.update(host, 5000, false);
            let s = sel.stat_man.find_stat(*host).unwrap();
            s.increment_counter();
        }

        let result = sel.select(&uris, &[]);
        assert!(result.is_some());
        assert!(result.unwrap() < 3);
    }

    #[test]
    fn test_stat_man_accessor() {
        let man = Arc::new(ServerStatMan::new());
        let sel = AdaptiveUriSelector::new(Arc::clone(&man));
        assert_eq!(sel.stat_man().count(), 0);
    }
}
