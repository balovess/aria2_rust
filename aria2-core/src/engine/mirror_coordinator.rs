// Mirror Coordinator - High-level abstraction for multi-mirror download coordination.
//
// This module provides a unified interface for coordinating downloads across
// multiple mirrors, integrating server statistics, URI selection, and segment
// management into a single cohesive API.

use std::sync::Arc;

use crate::constants;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

/// Configuration for mirror coordination.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// Maximum number of concurrent connections per mirror.
    pub max_connections_per_mirror: usize,
    /// Maximum total concurrent connections across all mirrors.
    pub max_total_connections: usize,
    /// Speed threshold in bytes/sec below which mirrors are deprioritized.
    pub speed_threshold: u64,
    /// Cooldown period in seconds for failed mirrors.
    pub cooldown_secs: u64,
    /// Maximum retries per segment before giving up.
    pub max_retries: u32,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            max_connections_per_mirror: constants::DEFAULT_MAX_CONNECTIONS_PER_MIRROR,
            max_total_connections: 16,
            speed_threshold: constants::MIRROR_SPEED_THRESHOLD,
            cooldown_secs: constants::MIRROR_COOLDOWN_SECS,
            max_retries: constants::MAX_MIRROR_FAILURES,
        }
    }
}

/// High-level coordinator for multi-mirror downloads.
///
/// This struct integrates `ServerStatMan`, `UriSelector`, and
/// `ConcurrentSegmentManager` to provide a unified API for
/// intelligent multi-mirror download coordination.
///
/// # Example
///
/// ```ignore
/// use std::sync::Arc;
/// use aria2_core::engine::mirror_coordinator::{MirrorCoordinator, MirrorConfig};
/// use aria2_core::selector::server_stat_man::ServerStatMan;
/// use aria2_core::selector::adaptive_uri_selector::AdaptiveUriSelector;
///
/// let stat_man = Arc::new(ServerStatMan::new());
/// let urls = vec!["http://mirror1.com/file".to_string()];
/// let selector = Box::new(AdaptiveUriSelector::new_with_uris(Arc::clone(&stat_man), urls.clone()));
///
/// let coordinator = MirrorCoordinator::new(
///     10_000_000,
///     urls,
///     Some(1_000_000),
///     stat_man,
///     selector,
///     MirrorConfig::default(),
/// );
/// ```
pub struct MirrorCoordinator {
    /// Server statistics manager for tracking mirror performance.
    stat_man: Arc<ServerStatMan>,
    /// URI selector for intelligent mirror selection.
    #[allow(dead_code)] // Selector stored for future per-request mirror selection logic
    selector: Box<dyn UriSelector>,
    /// Segment manager for download state tracking.
    segment_manager: ConcurrentSegmentManager,
    /// Configuration for coordination behavior.
    config: MirrorConfig,
    /// Mirror URLs.
    urls: Vec<String>,
}

impl MirrorCoordinator {
    /// Create a new mirror coordinator.
    ///
    /// # Arguments
    ///
    /// * `total_size` - Total file size in bytes
    /// * `urls` - List of mirror URLs
    /// * `segment_size` - Optional segment size (default: 1 MB)
    /// * `stat_man` - Shared server statistics manager
    /// * `selector` - URI selector for intelligent mirror selection
    /// * `config` - Configuration for coordination behavior
    pub fn new(
        total_size: u64,
        urls: Vec<String>,
        segment_size: Option<u64>,
        stat_man: Arc<ServerStatMan>,
        selector: Box<dyn UriSelector>,
        config: MirrorConfig,
    ) -> Self {
        // Create segment manager with the provided selector
        let segment_manager = ConcurrentSegmentManager::new_with_selector(
            total_size,
            urls.clone(),
            segment_size,
            Arc::clone(&stat_man),
            selector,
        );

        // Create a new selector for the coordinator (since the previous one was moved)
        // Note: In practice, you'd want to share the selector or use Arc
        let coordinator_selector =
            Box::new(crate::selector::uri_selector::InorderUriSelector::new());

        Self {
            stat_man,
            selector: coordinator_selector,
            segment_manager,
            config,
            urls,
        }
    }

    /// Create a mirror coordinator with a pre-configured segment manager.
    ///
    /// This is useful when you want more control over the segment manager
    /// configuration.
    pub fn with_segment_manager(
        stat_man: Arc<ServerStatMan>,
        selector: Box<dyn UriSelector>,
        segment_manager: ConcurrentSegmentManager,
        config: MirrorConfig,
        urls: Vec<String>,
    ) -> Self {
        Self {
            stat_man,
            selector,
            segment_manager,
            config,
            urls,
        }
    }

    /// Select a mirror for the next pending segment.
    ///
    /// This method uses the configured `UriSelector` to intelligently
    /// choose the best mirror based on historical performance data.
    ///
    /// # Returns
    ///
    /// * `Some((mirror_idx, mirror_url, segment_info))` - Selected mirror and segment info
    /// * `None` - No pending segments or no available mirrors
    pub fn select_mirror_for_segment(&mut self) -> Option<(usize, String, (u32, u64, u64))> {
        let result = self.segment_manager.select_mirror_for_next_segment()?;

        let (mirror_idx, seg_info) = result;
        let mirror_url = self.urls.get(mirror_idx).cloned()?;

        Some((mirror_idx, mirror_url, seg_info))
    }

    /// Report a successful segment download.
    ///
    /// This updates server statistics with the measured speed and
    /// may trigger connection rebalancing.
    ///
    /// # Arguments
    ///
    /// * `mirror_idx` - Index of the mirror that completed the download
    /// * `seg_idx` - Index of the completed segment
    /// * `data` - Downloaded data
    /// * `bytes_per_sec` - Measured download speed
    pub fn on_segment_complete(
        &mut self,
        mirror_idx: usize,
        seg_idx: u32,
        data: bytes::Bytes,
        bytes_per_sec: u64,
    ) -> bool {
        let is_multi = self.segment_manager.mirror_active_segments(mirror_idx) > 1;

        let success =
            self.segment_manager
                .report_segment_complete(seg_idx, data, bytes_per_sec, is_multi);

        if success {
            // Check if we should rebalance connections
            self.maybe_rebalance_connections();
        }

        success
    }

    /// Report a failed segment download.
    ///
    /// This updates server statistics and may disable the mirror
    /// if it has too many consecutive failures.
    ///
    /// # Arguments
    ///
    /// * `mirror_idx` - Index of the mirror that failed
    /// * `seg_idx` - Index of the failed segment
    /// * `error_code` - HTTP error code or 0 for network errors
    ///
    /// # Returns
    ///
    /// * `Some(new_mirror_idx)` - Segment reassigned to a new mirror
    /// * `None` - Segment permanently failed
    pub fn on_segment_failed(
        &mut self,
        _mirror_idx: usize,
        seg_idx: u32,
        error_code: u16,
    ) -> Option<usize> {
        self.segment_manager
            .report_segment_failed(seg_idx, error_code)
    }

    /// Get the maximum download speed across all mirrors.
    ///
    /// This is used as a baseline for calculating optimal connections.
    ///
    /// # Returns
    ///
    /// Maximum speed in bytes/sec, or 1 if no speed data available.
    fn get_max_mirror_speed(&self) -> u64 {
        self.urls
            .iter()
            .filter_map(|url| {
                let host = extract_host(url);
                self.stat_man
                    .find_stat(&host)
                    .map(|stat| stat.get_avg_speed())
            })
            .max()
            .unwrap_or(1)
    }

    /// Calculate the optimal number of connections for a specific mirror.
    ///
    /// This algorithm adjusts connections based on relative performance:
    /// - Fast mirrors (high speed ratio) get more connections
    /// - Slow mirrors (low speed ratio) get fewer connections
    ///
    /// # Algorithm
    ///
    /// ```text
    /// ratio = mirror_speed / max_speed  (0.0 ~ 1.0)
    /// optimal = base_connections + (ratio * 2.0)
    /// ```
    ///
    /// # Arguments
    ///
    /// * `mirror_idx` - Index of the mirror
    ///
    /// # Returns
    ///
    /// Optimal number of connections for this mirror.
    fn calculate_optimal_connections(&self, mirror_idx: usize) -> usize {
        let url = match self.urls.get(mirror_idx) {
            Some(u) => u,
            None => return self.config.max_connections_per_mirror,
        };

        let host = extract_host(url);
        let stat = match self.stat_man.find_stat(&host) {
            Some(s) => s,
            None => return self.config.max_connections_per_mirror,
        };

        let avg_speed = stat.get_avg_speed();
        let max_speed = self.get_max_mirror_speed();

        // Speed ratio 0.0 ~ 1.0
        let ratio = if max_speed > 0 {
            avg_speed as f64 / max_speed as f64
        } else {
            1.0
        };

        // Base connections + speed bonus
        let base = self.config.max_connections_per_mirror;
        let bonus = (ratio * 2.0) as usize;

        // Clamp to valid range
        (base + bonus).clamp(1, self.config.max_total_connections)
    }

    /// Check if connection rebalancing is needed and perform it.
    ///
    /// This method adjusts the maximum connections per mirror based
    /// on relative performance. Fast mirrors get more connections,
    /// slow mirrors get fewer.
    fn maybe_rebalance_connections(&mut self) {
        // Calculate optimal connections for each mirror
        for idx in 0..self.urls.len() {
            let optimal = self.calculate_optimal_connections(idx);

            // Update the segment manager's mirror state
            self.segment_manager
                .set_mirror_max_connections(idx, optimal);
        }
    }

    /// Rebalance connections across mirrors based on performance.
    ///
    /// This is a more aggressive rebalancing that should be called
    /// periodically (e.g., every 10 seconds) to adjust connections
    /// based on current performance.
    pub fn rebalance_connections(&mut self) {
        self.maybe_rebalance_connections();
    }

    /// Check if all segments are complete.
    pub fn is_complete(&self) -> bool {
        self.segment_manager.is_complete()
    }

    /// Check if there are any failed segments.
    pub fn has_failed_segments(&self) -> bool {
        self.segment_manager.has_failed_segments()
    }

    /// Get the completed ranges from the segment manager.
    ///
    /// Returns a vector of (offset, length) tuples representing segments that
    /// have been successfully downloaded.
    pub fn completed_ranges(&self) -> Vec<(u64, u64)> {
        self.segment_manager.completed_ranges()
    }

    /// Check if there are pending segments.
    pub fn has_pending_segments(&self) -> bool {
        self.segment_manager.has_pending_segments()
    }

    /// Get download progress as a percentage.
    pub fn progress(&self) -> f64 {
        self.segment_manager.progress()
    }

    /// Get total number of bytes downloaded across all completed segments.
    pub fn completed_bytes(&self) -> u64 {
        self.segment_manager.completed_bytes()
    }

    /// Get the total number of segments.
    pub fn num_segments(&self) -> usize {
        self.segment_manager.num_segments()
    }

    /// Get the number of mirrors.
    pub fn num_mirrors(&self) -> usize {
        self.urls.len()
    }

    /// Get the assembled data if download is complete.
    pub fn assemble(&self) -> Option<Vec<u8>> {
        self.segment_manager.assemble()
    }

    /// Get the server statistics manager.
    pub fn stat_man(&self) -> &Arc<ServerStatMan> {
        &self.stat_man
    }

    /// Get the configuration.
    pub fn config(&self) -> &MirrorConfig {
        &self.config
    }

    /// Save server statistics to a file.
    pub async fn save_stats(&self, path: &std::path::Path) -> Result<usize, String> {
        self.stat_man.save_to_file_async(path).await
    }

    /// Load server statistics from a file.
    pub async fn load_stats(&self, path: &std::path::Path) -> Result<usize, String> {
        self.stat_man.load_from_file_async(path).await
    }
}

/// Extract host from URL (helper function).
fn extract_host(url: &str) -> String {
    let url = url.trim();
    if !url.contains("://") {
        return url.to_string();
    }
    let after_scheme = &url[url.find("://").unwrap() + 3..];
    let host_part = if let Some(slash_idx) = after_scheme.find('/') {
        &after_scheme[..slash_idx]
    } else {
        after_scheme
    };
    host_part.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;

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

        let (mirror_idx, url, (seg_idx, offset, len)) = result.unwrap();
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
        let (mirror_idx, _, (seg_idx, _, _)) = coord.select_mirror_for_segment().unwrap();

        // Report completion
        let success = coord.on_segment_complete(
            mirror_idx,
            seg_idx,
            bytes::Bytes::from(vec![0xAB; 500_000]),
            1_000_000,
        );
        assert!(success);

        // Check progress
        assert!((coord.progress() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_on_segment_failed() {
        let mut coord = create_test_coordinator();

        // Select a segment first
        let (_, _, (seg_idx, _, _)) = coord.select_mirror_for_segment().unwrap();

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
        let (mirror_idx, _, (seg_idx, _, _)) = coord.select_mirror_for_segment().unwrap();
        coord.on_segment_complete(
            mirror_idx,
            seg_idx,
            bytes::Bytes::from(vec![0xAB; 500_000]),
            1_000_000,
        );
        assert!((coord.progress() - 50.0).abs() < 0.01);

        // Complete second segment
        let (mirror_idx2, _, (seg_idx2, _, _)) = coord.select_mirror_for_segment().unwrap();
        coord.on_segment_complete(
            mirror_idx2,
            seg_idx2,
            bytes::Bytes::from(vec![0xCD; 500_000]),
            1_000_000,
        );
        assert!((coord.progress() - 100.0).abs() < 0.01);
        assert!(coord.is_complete());
    }

    #[test]
    fn test_assemble() {
        let mut coord = create_test_coordinator();

        // Not complete yet
        assert!(coord.assemble().is_none());

        // Complete all segments
        let (m1, _, (s1, _, _)) = coord.select_mirror_for_segment().unwrap();
        coord.on_segment_complete(m1, s1, bytes::Bytes::from(vec![0xAB; 500_000]), 1_000_000);

        let (m2, _, (s2, _, _)) = coord.select_mirror_for_segment().unwrap();
        coord.on_segment_complete(m2, s2, bytes::Bytes::from(vec![0xCD; 500_000]), 1_000_000);

        // Now should be able to assemble
        let data = coord.assemble().unwrap();
        assert_eq!(data.len(), 1_000_000);
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
        assert_eq!(coord.segment_manager.get_mirror_max_connections(0), Some(2));
        assert_eq!(coord.segment_manager.get_mirror_max_connections(1), Some(2));

        // Rebalance
        coord.rebalance_connections();

        // After rebalance: fast mirror should have more connections
        let conn0 = coord.segment_manager.get_mirror_max_connections(0).unwrap();
        let conn1 = coord.segment_manager.get_mirror_max_connections(1).unwrap();
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
        let (mirror_idx, _, (seg_idx, _, _)) = coord.select_mirror_for_segment().unwrap();
        let success = coord.on_segment_complete(
            mirror_idx,
            seg_idx,
            bytes::Bytes::from(vec![0xAB; 500_000]),
            2_000_000,
        );
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
