// Tests for mirror coordinator.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
    use crate::engine::mirror_coordinator::config::MirrorConfig;
    use crate::engine::mirror_coordinator::coordinator::MirrorCoordinator;
    use crate::engine::mirror_coordinator::helpers::extract_host;
    use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;
    use crate::selector::server_stat_man::ServerStatMan;

    fn create_test_coordinator() -> MirrorCoordinator {
        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://mirror1.com/file".to_string(),
            "http://mirror2.com/file".to_string(),
        ];
        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let segment_manager = ConcurrentSegmentManager::new_with_selector(
            1_000_000,
            urls.clone(),
            Some(500_000),
            Arc::clone(&stat_man),
            selector,
        );

        // Create a new selector for the coordinator
        let selector2 = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        MirrorCoordinator::with_segment_manager(
            stat_man,
            selector2,
            segment_manager,
            MirrorConfig::default(),
            urls,
        )
    }

    #[test]
    fn test_coordinator_creation() {
        let coord = create_test_coordinator();
        assert_eq!(coord.num_segments(), 2);
        assert_eq!(coord.num_mirrors(), 2);
        assert!(!coord.is_complete());
        assert!(coord.has_pending_segments());
    }

    #[test]
    fn test_select_mirror_for_segment() {
        let mut coord = create_test_coordinator();
        let result = coord.select_mirror_for_segment();
        assert!(result.is_some());

        let (mirror_idx, url, (seg_idx, offset, len)) =
            result.expect("select_mirror_for_segment should return Some");
        assert!(mirror_idx < 2);
        assert!(url.contains("mirror"));
        assert_eq!(seg_idx, 0);
        assert_eq!(offset, 0);
        assert_eq!(len, 500_000);
    }

    #[test]
    fn test_on_segment_complete() {
        let mut coord = create_test_coordinator();

        // Select a segment first
        let (mirror_idx, _, (seg_idx, _, _)) = coord
            .select_mirror_for_segment()
            .expect("select_mirror_for_segment should return Some");

        // Report completion
        let success = coord.on_segment_complete(mirror_idx, seg_idx, 500_000, 1_000_000);
        assert!(success);

        // Check progress
        assert!((coord.progress() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_on_segment_failed() {
        let mut coord = create_test_coordinator();

        // Select a segment first
        let (_, _, (seg_idx, _, _)) = coord
            .select_mirror_for_segment()
            .expect("select_mirror_for_segment should return Some");

        // Report failure
        let reassign = coord.on_segment_failed(0, seg_idx, 503);
        assert!(reassign.is_some());
    }

    #[test]
    fn test_mirror_config_default() {
        let config = MirrorConfig::default();
        assert_eq!(config.max_connections_per_mirror, 2);
        assert_eq!(config.max_total_connections, 16);
        assert_eq!(config.speed_threshold, 10_000);
        assert_eq!(config.cooldown_secs, 60);
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn test_extract_host() {
        assert_eq!(extract_host("http://example.com/path"), "example.com");
        assert_eq!(extract_host("https://host:8080/file"), "host:8080");
        assert_eq!(extract_host("ftp://server.com"), "server.com");
    }

    #[test]
    fn test_progress_tracking() {
        let mut coord = create_test_coordinator();

        assert_eq!(coord.progress(), 0.0);

        // Complete first segment
        let (mirror_idx, _, (seg_idx, _, _)) = coord
            .select_mirror_for_segment()
            .expect("select_mirror_for_segment should return Some");
        coord.on_segment_complete(mirror_idx, seg_idx, 500_000, 1_000_000);
        assert!((coord.progress() - 50.0).abs() < 0.01);

        // Complete second segment
        let (mirror_idx2, _, (seg_idx2, _, _)) = coord
            .select_mirror_for_segment()
            .expect("select_mirror_for_segment should return Some");
        coord.on_segment_complete(mirror_idx2, seg_idx2, 500_000, 1_000_000);
        assert!((coord.progress() - 100.0).abs() < 0.01);
        assert!(coord.is_complete());
    }

    #[test]
    fn test_calculate_optimal_connections() {
        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://fast.com/file".to_string(),
            "http://slow.com/file".to_string(),
        ];

        // Setup: fast mirror has 2MB/s, slow mirror has 500KB/s
        stat_man.update("fast.com", 2_000_000, false);
        stat_man.update("slow.com", 500_000, false);

        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let segment_manager = ConcurrentSegmentManager::new_with_selector(
            1_000_000,
            urls.clone(),
            Some(500_000),
            Arc::clone(&stat_man),
            selector,
        );

        let selector2 = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let coord = MirrorCoordinator::with_segment_manager(
            stat_man,
            selector2,
            segment_manager,
            MirrorConfig::default(),
            urls,
        );

        // Fast mirror (ratio = 1.0) should get base + 2 = 4 connections
        let fast_optimal = coord.calculate_optimal_connections(0);
        assert_eq!(fast_optimal, 4);

        // Slow mirror (ratio = 0.25) should get base + 0 = 2 connections
        let slow_optimal = coord.calculate_optimal_connections(1);
        assert_eq!(slow_optimal, 2);
    }

    #[test]
    fn test_rebalance_connections() {
        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://mirror1.com/file".to_string(),
            "http://mirror2.com/file".to_string(),
        ];

        // Setup different speeds
        stat_man.update("mirror1.com", 1_500_000, false);
        stat_man.update("mirror2.com", 500_000, false);

        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let mut segment_manager = ConcurrentSegmentManager::new_with_selector(
            1_000_000,
            urls.clone(),
            Some(500_000),
            Arc::clone(&stat_man),
            selector,
        );

        // Set initial connections
        segment_manager.set_mirror_max_connections(0, 2);
        segment_manager.set_mirror_max_connections(1, 2);

        let selector2 = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let mut coord = MirrorCoordinator::with_segment_manager(
            stat_man,
            selector2,
            segment_manager,
            MirrorConfig::default(),
            urls,
        );

        // Before rebalance
        assert_eq!(
            coord.segment_manager().get_mirror_max_connections(0),
            Some(2)
        );
        assert_eq!(
            coord.segment_manager().get_mirror_max_connections(1),
            Some(2)
        );

        // Rebalance
        coord.rebalance_connections();

        // After rebalance: fast mirror should have more connections
        let conn0 = coord
            .segment_manager()
            .get_mirror_max_connections(0)
            .expect("mirror 0 should have max connections");
        let conn1 = coord
            .segment_manager()
            .get_mirror_max_connections(1)
            .expect("mirror 1 should have max connections");
        assert!(
            conn0 >= conn1,
            "Fast mirror should have >= connections than slow mirror"
        );
    }

    #[test]
    fn test_dynamic_connection_adjustment_on_completion() {
        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://fast.com/file".to_string(),
            "http://slow.com/file".to_string(),
        ];

        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let segment_manager = ConcurrentSegmentManager::new_with_selector(
            1_000_000,
            urls.clone(),
            Some(500_000),
            Arc::clone(&stat_man),
            selector,
        );

        let selector2 = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let mut coord = MirrorCoordinator::with_segment_manager(
            stat_man,
            selector2,
            segment_manager,
            MirrorConfig::default(),
            urls,
        );

        // Select and complete segment from fast mirror
        let (mirror_idx, _, (seg_idx, _, _)) = coord
            .select_mirror_for_segment()
            .expect("select_mirror_for_segment should return Some");
        let success = coord.on_segment_complete(mirror_idx, seg_idx, 500_000, 2_000_000);
        assert!(success);

        // After completion, the coordinator should have updated stats
        // The fast mirror should now have higher optimal connections
        let optimal = coord.calculate_optimal_connections(mirror_idx);
        assert!(
            optimal >= 2,
            "Optimal connections should be at least base value"
        );
    }

    #[test]
    fn test_max_mirror_speed() {
        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://mirror1.com/file".to_string(),
            "http://mirror2.com/file".to_string(),
            "http://mirror3.com/file".to_string(),
        ];

        // Setup: mirror1=1MB/s, mirror2=2MB/s, mirror3=500KB/s
        // Use multiple updates to let EMA converge closer to target values
        for _ in 0..10 {
            stat_man.update("mirror1.com", 1_000_000, false);
            stat_man.update("mirror2.com", 2_000_000, false);
            stat_man.update("mirror3.com", 500_000, false);
        }

        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let segment_manager = ConcurrentSegmentManager::new_with_selector(
            1_000_000,
            urls.clone(),
            Some(500_000),
            Arc::clone(&stat_man),
            selector,
        );

        let selector2 = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let coord = MirrorCoordinator::with_segment_manager(
            stat_man,
            selector2,
            segment_manager,
            MirrorConfig::default(),
            urls,
        );

        // Max speed should be close to 2MB/s (mirror2)
        // Due to EMA, it won't be exactly 2MB/s but should be the highest
        let max_speed = coord.get_max_mirror_speed();
        assert!(
            max_speed > 1_500_000,
            "Max speed should be > 1.5MB/s, got {}",
            max_speed
        );
        assert!(
            max_speed < 2_100_000,
            "Max speed should be < 2.1MB/s, got {}",
            max_speed
        );
    }
}
