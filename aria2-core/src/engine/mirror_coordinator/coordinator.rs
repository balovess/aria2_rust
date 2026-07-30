// Mirror Coordinator - Core coordinator struct and implementation.
//
// This module provides the `MirrorCoordinator` struct which integrates
// `ServerStatMan`, `UriSelector`, and `ConcurrentSegmentManager` into
// a unified API for intelligent multi-mirror download coordination.

use std::sync::Arc;

use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

use super::config::MirrorConfig;
use super::helpers::extract_host;

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
    /// * `len` - Length of downloaded data in bytes
    /// * `bytes_per_sec` - Measured download speed
    pub fn on_segment_complete(
        &mut self,
        mirror_idx: usize,
        seg_idx: u32,
        len: usize,
        bytes_per_sec: u64,
    ) -> bool {
        let is_multi = self.segment_manager.mirror_active_segments(mirror_idx) > 1;

        let success =
            self.segment_manager
                .report_segment_complete(seg_idx, len, bytes_per_sec, is_multi);

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
    pub fn get_max_mirror_speed(&self) -> u64 {
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
    pub(crate) fn calculate_optimal_connections(&self, mirror_idx: usize) -> usize {
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

    /// Get the server statistics manager.
    pub fn stat_man(&self) -> &Arc<ServerStatMan> {
        &self.stat_man
    }

    /// Get the segment manager (crate-internal access for testing).
    pub(crate) fn segment_manager(&self) -> &ConcurrentSegmentManager {
        &self.segment_manager
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
