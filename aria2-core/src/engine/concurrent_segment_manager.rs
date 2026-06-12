use std::sync::Arc;
use tracing::debug;

use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

#[derive(Debug, Clone, PartialEq)]
pub enum SegmentStatus {
    Pending,
    Downloading,
    Done,
    Failed,
}

#[derive(Debug)]
pub struct Segment {
    pub index: u32,
    pub offset: u64,
    pub length: u64,
    pub status: SegmentStatus,
    pub data: Option<bytes::Bytes>,  // Zero-copy storage
    pub assigned_mirror: Option<usize>,
    pub retry_count: u32,
}

impl Segment {
    fn new(index: u32, offset: u64, length: u64) -> Self {
        Self {
            index,
            offset,
            length,
            status: SegmentStatus::Pending,
            data: None,
            assigned_mirror: None,
            retry_count: 0,
        }
    }
}

#[derive(Debug)]
pub struct MirrorState {
    pub url: String,
    pub speed: f64,
    pub active_segments: usize,
    pub max_connections: usize,
    pub consecutive_failures: usize,
    pub disabled: bool,
}

impl MirrorState {
    fn new(url: String) -> Self {
        Self {
            url,
            speed: 0.0,
            active_segments: 0,
            max_connections: 2,
            consecutive_failures: 0,
            disabled: false,
        }
    }

    pub fn is_available(&self) -> bool {
        !self.disabled && self.active_segments < self.max_connections
    }

    pub fn can_accept_more(&self) -> bool {
        !self.disabled && self.active_segments < self.max_connections
    }
}

pub struct ConcurrentSegmentManager {
    total_size: u64,
    #[allow(dead_code)] // Reserved for segment size queries in future optimizations
    segment_size: u64,
    segments: Vec<Segment>,
    mirrors: Vec<MirrorState>,
    /// URLs for mirror selection (used with UriSelector)
    mirror_urls: Vec<String>,
    completed_bytes: u64,
    max_retries_per_segment: u32,
    max_mirror_failures: usize,
    /// Optional server statistics manager for intelligent mirror selection
    stat_man: Option<Arc<ServerStatMan>>,
    /// Optional URI selector for intelligent mirror selection
    uri_selector: Option<Box<dyn UriSelector>>,
}

impl ConcurrentSegmentManager {
    /// Create a new segment manager with basic round-robin mirror selection.
    ///
    /// This is the basic constructor that uses the built-in `MirrorState`
    /// for mirror tracking. For intelligent mirror selection, use
    /// `new_with_selector()` instead.
    pub fn new(total_size: u64, urls: Vec<String>, segment_size: Option<u64>) -> Self {
        let seg_size = segment_size.unwrap_or(1_048_576);
        let num_segments = if total_size == 0 {
            0
        } else {
            total_size.div_ceil(seg_size) as usize
        };

        let mut segments = Vec::with_capacity(num_segments);
        for i in 0..num_segments {
            let offset = (i as u64) * seg_size;
            let remaining = total_size.saturating_sub(offset);
            let length = seg_size.min(remaining);
            segments.push(Segment::new(i as u32, offset, length));
        }

        let mirrors = urls.iter().cloned().map(MirrorState::new).collect();

        Self {
            total_size,
            segment_size: seg_size,
            segments,
            mirrors,
            mirror_urls: urls,
            completed_bytes: 0,
            max_retries_per_segment: 3,
            max_mirror_failures: 3,
            stat_man: None,
            uri_selector: None,
        }
    }

    /// Create a new segment manager with intelligent mirror selection.
    ///
    /// This constructor integrates with `ServerStatMan` and `UriSelector`
    /// for intelligent mirror selection based on historical performance data.
    ///
    /// # Arguments
    ///
    /// * `total_size` - Total file size in bytes
    /// * `urls` - List of mirror URLs
    /// * `segment_size` - Optional segment size (default: 1 MB)
    /// * `stat_man` - Shared server statistics manager
    /// * `uri_selector` - URI selector for intelligent mirror selection
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::sync::Arc;
    /// use aria2_core::selector::server_stat_man::ServerStatMan;
    /// use aria2_core::selector::adaptive_uri_selector::AdaptiveUriSelector;
    /// use aria2_core::engine::concurrent_segment_manager::ConcurrentSegmentManager;
    ///
    /// let stat_man = Arc::new(ServerStatMan::new());
    /// let urls = vec!["http://mirror1.com/file".to_string()];
    /// let selector = Box::new(AdaptiveUriSelector::new_with_uris(Arc::clone(&stat_man), urls.clone()));
    ///
    /// let mgr = ConcurrentSegmentManager::new_with_selector(
    ///     10_000_000,
    ///     urls,
    ///     Some(1_000_000),
    ///     stat_man,
    ///     selector,
    /// );
    /// ```
    pub fn new_with_selector(
        total_size: u64,
        urls: Vec<String>,
        segment_size: Option<u64>,
        stat_man: Arc<ServerStatMan>,
        uri_selector: Box<dyn UriSelector>,
    ) -> Self {
        let seg_size = segment_size.unwrap_or(1_048_576);
        let num_segments = if total_size == 0 {
            0
        } else {
            total_size.div_ceil(seg_size) as usize
        };

        let mut segments = Vec::with_capacity(num_segments);
        for i in 0..num_segments {
            let offset = (i as u64) * seg_size;
            let remaining = total_size.saturating_sub(offset);
            let length = seg_size.min(remaining);
            segments.push(Segment::new(i as u32, offset, length));
        }

        let mirrors = urls.iter().cloned().map(MirrorState::new).collect();

        Self {
            total_size,
            segment_size: seg_size,
            segments,
            mirrors,
            mirror_urls: urls,
            completed_bytes: 0,
            max_retries_per_segment: 3,
            max_mirror_failures: 3,
            stat_man: Some(stat_man),
            uri_selector: Some(uri_selector),
        }
    }

    pub fn allocate_segments(&mut self) {
        for mirror_idx in 0..self.mirrors.len() {
            while self.mirrors[mirror_idx].can_accept_more() {
                if let Some(seg) = self.find_pending_segment() {
                    seg.status = SegmentStatus::Downloading;
                    seg.assigned_mirror = Some(mirror_idx);
                    self.mirrors[mirror_idx].active_segments += 1;
                } else {
                    break;
                }
            }
        }
    }

    fn find_pending_segment(&mut self) -> Option<&mut Segment> {
        self.segments
            .iter_mut()
            .find(|s| s.status == SegmentStatus::Pending)
    }

    pub fn next_pending_segment_for_mirror(
        &mut self,
        mirror_idx: usize,
    ) -> Option<(u32, u64, u64)> {
        if !self
            .mirrors
            .get(mirror_idx)
            .is_some_and(|m| m.can_accept_more())
        {
            return None;
        }

        for seg in &mut self.segments {
            if seg.status == SegmentStatus::Pending {
                seg.status = SegmentStatus::Downloading;
                seg.assigned_mirror = Some(mirror_idx);
                if let Some(m) = self.mirrors.get_mut(mirror_idx) {
                    m.active_segments += 1;
                }
                return Some((seg.index, seg.offset, seg.length));
            }
        }
        None
    }

    pub fn next_pending_segment(&mut self) -> Option<(u32, u64, u64)> {
        self.next_pending_segment_for_mirror(0)
    }

    pub fn complete_segment(&mut self, index: u32, data: bytes::Bytes) -> bool {
        if let Some(seg) = self.segments.get_mut(index as usize) {
            seg.status = SegmentStatus::Done;
            seg.data = Some(data);

            if let Some(mi) = seg.assigned_mirror
                && let Some(m) = self.mirrors.get_mut(mi)
            {
                m.active_segments = m.active_segments.saturating_sub(1);
                m.consecutive_failures = 0;
            }

            self.completed_bytes += seg.length;
            true
        } else {
            false
        }
    }

    pub fn fail_segment(&mut self, index: u32) -> Option<usize> {
        let (prev_mirror, new_retry) = {
            let seg = self.segments.get(index as usize)?;
            (seg.assigned_mirror, seg.retry_count + 1)
        };

        if let Some(mi) = prev_mirror
            && let Some(m) = self.mirrors.get_mut(mi)
        {
            m.active_segments = m.active_segments.saturating_sub(1);
            m.consecutive_failures += 1;
            if m.consecutive_failures >= self.max_mirror_failures {
                m.disabled = true;
            }
        }

        if new_retry >= self.max_retries_per_segment {
            if let Some(seg) = self.segments.get_mut(index as usize) {
                seg.status = SegmentStatus::Failed;
                seg.retry_count = new_retry;
            }
            None
        } else {
            let reassign = self.find_available_mirror_for_reassignment(prev_mirror.unwrap_or(0));
            if let Some(seg) = self.segments.get_mut(index as usize) {
                seg.status = SegmentStatus::Pending;
                seg.assigned_mirror = reassign;
                seg.retry_count = new_retry;
            }
            reassign
        }
    }

    fn find_available_mirror_for_reassignment(&self, exclude: usize) -> Option<usize> {
        self.mirrors
            .iter()
            .enumerate()
            .filter(|(i, m)| *i != exclude && m.is_available())
            .map(|(i, _)| i)
            .next()
    }

    pub fn is_complete(&self) -> bool {
        self.segments
            .iter()
            .all(|s| s.status == SegmentStatus::Done)
    }

    pub fn has_failed_segments(&self) -> bool {
        self.segments
            .iter()
            .any(|s| s.status == SegmentStatus::Failed)
    }

    pub fn has_pending_segments(&self) -> bool {
        self.segments
            .iter()
            .any(|s| s.status == SegmentStatus::Pending)
    }

    pub fn assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() || self.total_size == 0 {
            return None;
        }

        let mut result = Vec::with_capacity(self.total_size as usize);
        for seg in &self.segments {
            let data = seg.data.as_ref()?;
            result.extend_from_slice(data);
        }
        Some(result)
    }

    pub fn progress(&self) -> f64 {
        if self.total_size == 0 {
            return 100.0;
        }
        let done = self
            .segments
            .iter()
            .filter(|s| s.status == SegmentStatus::Done)
            .count();
        done as f64 / self.segments.len() as f64 * 100.0
    }

    pub fn num_segments(&self) -> usize {
        self.segments.len()
    }
    pub fn segment_status(&self, index: usize) -> Option<SegmentStatus> {
        self.segments.get(index).map(|s| s.status.clone())
    }
    pub fn num_mirrors(&self) -> usize {
        self.mirrors.len()
    }
    pub fn total_size(&self) -> u64 {
        self.total_size
    }
    pub fn completed_bytes(&self) -> u64 {
        self.completed_bytes
    }

    pub fn mirror_url(&self, index: usize) -> Option<&str> {
        self.mirrors.get(index).map(|m| m.url.as_str())
    }

    pub fn available_mirrors(&self) -> Vec<usize> {
        self.mirrors
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_available())
            .map(|(i, _)| i)
            .collect()
    }

    pub fn any_mirror_available(&self) -> bool {
        self.mirrors.iter().any(|m| m.is_available())
    }

    pub fn set_max_connections_per_mirror(&mut self, max: usize) {
        for m in &mut self.mirrors {
            m.max_connections = max;
        }
    }

    /// Set the maximum connections for a specific mirror.
    ///
    /// This is used for dynamic connection rebalancing based on mirror performance.
    ///
    /// # Arguments
    ///
    /// * `mirror_idx` - Index of the mirror
    /// * `max` - New maximum connections for this mirror
    pub fn set_mirror_max_connections(&mut self, mirror_idx: usize, max: usize) {
        if let Some(mirror) = self.mirrors.get_mut(mirror_idx) {
            mirror.max_connections = max;
        }
    }

    /// Get the current maximum connections for a specific mirror.
    ///
    /// # Arguments
    ///
    /// * `mirror_idx` - Index of the mirror
    pub fn get_mirror_max_connections(&self, mirror_idx: usize) -> Option<usize> {
        self.mirrors.get(mirror_idx).map(|m| m.max_connections)
    }

    pub fn set_max_retries(&mut self, retries: u32) {
        self.max_retries_per_segment = retries;
    }

    pub fn segment_retry_count(&self, seg_idx: u32) -> u32 {
        self.segments
            .iter()
            .find(|s| s.index == seg_idx)
            .map(|s| s.retry_count)
            .unwrap_or(0)
    }

    pub fn has_permanently_failed_segments(&self) -> bool {
        self.segments.iter().any(|s| {
            s.status == SegmentStatus::Failed && s.retry_count >= self.max_retries_per_segment
        })
    }

    pub fn mark_completed_up_to(&mut self, offset: u64, length: u64) {
        let end_offset = offset + length;
        for segment in &mut self.segments {
            if segment.offset + segment.length <= offset {
                if segment.status != SegmentStatus::Done {
                    segment.status = SegmentStatus::Done;
                    self.completed_bytes += segment.length;
                }
            } else if segment.offset < end_offset {
                let overlap_start = std::cmp::max(segment.offset, offset);
                let overlap_end = std::cmp::min(segment.offset + segment.length, end_offset);
                if overlap_end > overlap_start {
                    debug!(
                        "段 {} 部分已完成: {}/{} bytes",
                        segment.index,
                        overlap_end - segment.offset,
                        segment.length
                    );
                }
            }
        }
    }

    pub fn segment_info(&self, index: usize) -> Option<(u64, u64, &SegmentStatus)> {
        self.segments
            .get(index)
            .map(|s| (s.offset, s.length, &s.status))
    }

    // ==================== Intelligent Mirror Selection Methods ====================

    /// Select a mirror for the next pending segment using UriSelector.
    ///
    /// If a `UriSelector` is configured, this method uses it to intelligently
    /// select the best mirror based on historical performance data. Otherwise,
    /// it falls back to the first available mirror.
    ///
    /// # Returns
    ///
    /// * `Some((mirror_idx, segment_info))` - Mirror index and segment info (index, offset, length)
    /// * `None` - No pending segments or no available mirrors
    pub fn select_mirror_for_next_segment(&mut self) -> Option<(usize, (u32, u64, u64))> {
        // Find a pending segment first
        let pending_seg = self
            .segments
            .iter()
            .find(|s| s.status == SegmentStatus::Pending)?;

        let seg_index = pending_seg.index;

        // Use UriSelector if available
        if let Some(ref selector) = self.uri_selector {
            // Build list of currently used hosts
            let used_hosts: Vec<(usize, String)> = self
                .segments
                .iter()
                .filter(|s| s.status == SegmentStatus::Downloading)
                .filter_map(|s| {
                    s.assigned_mirror.and_then(|idx| {
                        self.mirror_urls.get(idx).map(|url| {
                            // Extract host from URL
                            let host = extract_host_from_url(url);
                            (idx, host)
                        })
                    })
                })
                .collect();

            // Select mirror using UriSelector
            if let Some(mirror_idx) = selector.select(&self.mirror_urls, &used_hosts) {
                // Check if mirror can accept more
                if self.mirrors.get(mirror_idx).is_some_and(|m| m.can_accept_more()) {
                    // Assign segment to this mirror
                    if let Some(seg) = self.segments.get_mut(seg_index as usize) {
                        seg.status = SegmentStatus::Downloading;
                        seg.assigned_mirror = Some(mirror_idx);
                        if let Some(m) = self.mirrors.get_mut(mirror_idx) {
                            m.active_segments += 1;
                        }
                        return Some((mirror_idx, (seg.index, seg.offset, seg.length)));
                    }
                }
            }
        }

        // Fallback: find first available mirror
        for mirror_idx in 0..self.mirrors.len() {
            if self.mirrors[mirror_idx].can_accept_more()
                && let Some(seg) = self.segments.get_mut(seg_index as usize)
            {
                seg.status = SegmentStatus::Downloading;
                seg.assigned_mirror = Some(mirror_idx);
                self.mirrors[mirror_idx].active_segments += 1;
                return Some((mirror_idx, (seg.index, seg.offset, seg.length)));
            }
        }

        None
    }

    /// Report a segment download completion with speed feedback.
    ///
    /// This method should be called after a segment is successfully downloaded.
    /// It updates the server statistics with the measured download speed,
    /// which affects future mirror selection decisions.
    ///
    /// # Arguments
    ///
    /// * `seg_idx` - Index of the completed segment
    /// * `data` - Downloaded data
    /// * `bytes_per_sec` - Measured download speed in bytes per second
    /// * `is_multi_connection` - Whether this was a multi-connection download
    ///
    /// # Returns
    ///
    /// `true` if the segment was successfully marked as complete
    pub fn report_segment_complete(
        &mut self,
        seg_idx: u32,
        data: bytes::Bytes,
        bytes_per_sec: u64,
        is_multi_connection: bool,
    ) -> bool {
        // Get the mirror index before completing
        let mirror_idx = self
            .segments
            .get(seg_idx as usize)
            .and_then(|s| s.assigned_mirror);

        // Complete the segment
        let success = self.complete_segment(seg_idx, data);

        // Update server stats if available
        if success {
            if let (Some(idx), Some(stat_man)) = (mirror_idx, &self.stat_man)
                && let Some(url) = self.mirror_urls.get(idx)
            {
                let host = extract_host_from_url(url);
                stat_man.update(&host, bytes_per_sec, is_multi_connection);

                // Reset failure count on success
                if let Some(stat) = stat_man.find_stat(&host) {
                    stat.reset_status();
                }
            }

            // Tune the selector with the new speed
            if let Some(ref selector) = self.uri_selector {
                selector.tune_command(&self.mirror_urls, bytes_per_sec);
            }
        }

        success
    }

    /// Report a segment download failure.
    ///
    /// This method should be called when a segment download fails.
    /// It updates the server statistics and may disable the mirror
    /// if it has too many consecutive failures.
    ///
    /// # Arguments
    ///
    /// * `seg_idx` - Index of the failed segment
    /// * `error_code` - HTTP error code (e.g., 500, 503) or 0 for network errors
    ///
    /// # Returns
    ///
    /// * `Some(new_mirror_idx)` - Segment reassigned to a new mirror
    /// * `None` - Segment permanently failed or no alternative mirror
    pub fn report_segment_failed(&mut self, seg_idx: u32, error_code: u16) -> Option<usize> {
        // Get the mirror index before failing
        let mirror_idx = self
            .segments
            .get(seg_idx as usize)
            .and_then(|s| s.assigned_mirror);

        // Fail the segment (this handles reassignment logic)
        let reassign = self.fail_segment(seg_idx);

        // Update server stats if available
        if let (Some(idx), Some(stat_man)) = (mirror_idx, &self.stat_man)
            && let Some(url) = self.mirror_urls.get(idx)
        {
            let host = extract_host_from_url(url);
            // Ensure stat exists before marking failure
            stat_man.get_or_create(&host);
            stat_man.mark_failure(&host, error_code);

            // Check if mirror should be disabled
            if let Some(stat) = stat_man.find_stat(&host)
                && !stat.is_available()
            {
                // Mirror is in cooldown, disable it temporarily
                if let Some(m) = self.mirrors.get_mut(idx) {
                    m.disabled = true;
                }
            }
        }

        reassign
    }

    /// Get the URL for a specific mirror index.
    pub fn get_mirror_url(&self, mirror_idx: usize) -> Option<&str> {
        self.mirror_urls.get(mirror_idx).map(|s| s.as_str())
    }

    /// Get the number of active segments for a specific mirror.
    pub fn mirror_active_segments(&self, mirror_idx: usize) -> usize {
        self.mirrors
            .get(mirror_idx)
            .map(|m| m.active_segments)
            .unwrap_or(0)
    }

    /// Check if the manager is using intelligent mirror selection.
    pub fn has_intelligent_selection(&self) -> bool {
        self.uri_selector.is_some() && self.stat_man.is_some()
    }
}

/// Extract host from URL (helper function).
fn extract_host_from_url(url: &str) -> String {
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

    #[test]
    fn test_manager_creation_small_file() {
        let mgr = ConcurrentSegmentManager::new(1024, vec!["http://a.com/f".to_string()], None);
        assert_eq!(mgr.num_segments(), 1);
        assert_eq!(mgr.num_mirrors(), 1);
        assert_eq!(mgr.total_size(), 1024);
        assert!(!mgr.is_complete());
        assert!(mgr.has_pending_segments());
    }

    #[test]
    fn test_manager_large_file_multi_segment() {
        let mgr = ConcurrentSegmentManager::new(
            3_000_000,
            vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
            Some(1_000_000),
        );
        assert_eq!(mgr.num_segments(), 3);
        assert_eq!(mgr.num_mirrors(), 2);
    }

    #[test]
    fn test_allocate_segments_round_robin() {
        let mut mgr = ConcurrentSegmentManager::new(
            3_000_000,
            vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
            Some(1_000_000),
        );

        mgr.allocate_segments();

        let assigned_a: Vec<_> = mgr
            .segments
            .iter()
            .filter(|s| s.assigned_mirror == Some(0))
            .map(|s| s.index)
            .collect();
        let assigned_b: Vec<_> = mgr
            .segments
            .iter()
            .filter(|s| s.assigned_mirror == Some(1))
            .map(|s| s.index)
            .collect();

        assert!(!assigned_a.is_empty());
        assert!(!assigned_b.is_empty());
        assert_eq!(assigned_a.len() + assigned_b.len(), 3);
    }

    #[test]
    fn test_complete_and_assemble() {
        let mut mgr =
            ConcurrentSegmentManager::new(200, vec!["http://x.com/f".to_string()], Some(100));

        mgr.allocate_segments();
        assert_eq!(mgr.progress(), 0.0);

        mgr.complete_segment(0, bytes::Bytes::from(vec![0xAB; 100]));
        assert!(!mgr.is_complete());
        assert!((mgr.progress() - 50.0).abs() < 0.01);

        mgr.complete_segment(1, bytes::Bytes::from(vec![0xCD; 100]));
        assert!(mgr.is_complete());
        assert!((mgr.progress() - 100.0).abs() < 0.01);

        let assembled = mgr.assemble().unwrap();
        assert_eq!(assembled.len(), 200);
        assert_eq!(&assembled[..100], &[0xAB; 100][..]);
        assert_eq!(&assembled[100..], &[0xCD; 100][..]);
    }

    #[test]
    fn test_fail_and_reassign() {
        let mut mgr = ConcurrentSegmentManager::new(
            200,
            vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
            Some(100),
        );

        mgr.allocate_segments();

        let reassign = mgr.fail_segment(0);
        assert!(reassign.is_some());

        let seg = &mgr.segments[0];
        assert_eq!(seg.status, SegmentStatus::Pending);
        assert_eq!(seg.assigned_mirror, reassign);
        assert_eq!(seg.retry_count, 1);
    }

    #[test]
    fn test_max_retries_exhausted() {
        let mut mgr =
            ConcurrentSegmentManager::new(100, vec!["http://a.com/f".to_string()], Some(100));
        mgr.set_max_retries(2);

        mgr.fail_segment(0);
        assert!(mgr.has_pending_segments());

        mgr.fail_segment(0);
        assert!(mgr.has_failed_segments());
        assert!(!mgr.has_pending_segments());
    }

    #[test]
    fn test_empty_file() {
        let mgr = ConcurrentSegmentManager::new(0, vec!["http://x.com/f".to_string()], None);
        assert_eq!(mgr.num_segments(), 0);
        assert!(mgr.is_complete());
        assert!(mgr.assemble().is_none());
    }

    #[test]
    fn test_next_pending_for_specific_mirror() {
        let mut mgr = ConcurrentSegmentManager::new(
            300,
            vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
            Some(100),
        );

        let r = mgr.next_pending_segment_for_mirror(0);
        assert!(r.is_some());
        let (idx, off, len) = r.unwrap();
        assert_eq!(idx, 0);
        assert_eq!(off, 0);
        assert_eq!(len, 100);

        let r2 = mgr.next_pending_segment_for_mirror(1);
        assert!(r2.is_some());
        let (idx2, _, _) = r2.unwrap();
        assert_eq!(idx2, 1);
    }

    // ======================================================================
    // Tests for Intelligent Mirror Selection
    // ======================================================================

    #[test]
    fn test_new_with_selector() {
        use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;

        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://mirror1.com/file".to_string(),
            "http://mirror2.com/file".to_string(),
        ];
        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let mgr = ConcurrentSegmentManager::new_with_selector(
            1_000_000,
            urls,
            Some(500_000),
            stat_man,
            selector,
        );

        assert_eq!(mgr.num_segments(), 2);
        assert_eq!(mgr.num_mirrors(), 2);
        assert!(mgr.has_intelligent_selection());
    }

    #[test]
    fn test_select_mirror_for_next_segment_without_selector() {
        let mut mgr = ConcurrentSegmentManager::new(
            300,
            vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
            Some(100),
        );

        // Without UriSelector, should use fallback
        let result = mgr.select_mirror_for_next_segment();
        assert!(result.is_some());

        let (mirror_idx, (seg_idx, offset, len)) = result.unwrap();
        assert_eq!(seg_idx, 0);
        assert_eq!(offset, 0);
        assert_eq!(len, 100);
        assert!(mirror_idx < 2);
    }

    #[test]
    fn test_select_mirror_for_next_segment_with_selector() {
        use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;

        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://fast.com/f".to_string(),
            "http://slow.com/f".to_string(),
        ];

        // Make fast.com have better stats
        stat_man.update("fast.com", 1_000_000, false);
        stat_man.update("slow.com", 1000, false);
        let fast_stat = stat_man.find_stat("fast.com").unwrap();
        fast_stat.increment_counter();
        let slow_stat = stat_man.find_stat("slow.com").unwrap();
        slow_stat.increment_counter();

        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let mut mgr = ConcurrentSegmentManager::new_with_selector(
            300,
            urls,
            Some(100),
            stat_man,
            selector,
        );

        let result = mgr.select_mirror_for_next_segment();
        assert!(result.is_some());

        let (mirror_idx, _) = result.unwrap();
        // Fast mirror (index 0) should be selected
        assert_eq!(mirror_idx, 0);
    }

    #[test]
    fn test_report_segment_complete_updates_stats() {
        use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;

        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec!["http://test.mirror.com/f".to_string()];

        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let mut mgr = ConcurrentSegmentManager::new_with_selector(
            100,
            urls,
            Some(100),
            stat_man.clone(),
            selector,
        );

        mgr.allocate_segments();

        // Report completion with 1 MB/s speed
        let success = mgr.report_segment_complete(0, bytes::Bytes::from(vec![0xAB; 100]), 1_000_000, false);
        assert!(success);

        // Check that stats were updated
        let stat = stat_man.find_stat("test.mirror.com").unwrap();
        assert!(stat.get_download_speed() > 0);
    }

    #[test]
    fn test_report_segment_failed_updates_stats() {
        use crate::selector::adaptive_uri_selector::AdaptiveUriSelector;

        let stat_man = Arc::new(ServerStatMan::new());
        let urls = vec![
            "http://failing.mirror.com/f".to_string(),
            "http://backup.mirror.com/f".to_string(),
        ];

        let selector = Box::new(AdaptiveUriSelector::new_with_uris(
            Arc::clone(&stat_man),
            urls.clone(),
        ));

        let mut mgr = ConcurrentSegmentManager::new_with_selector(
            100,
            urls,
            Some(100),
            stat_man.clone(),
            selector,
        );

        mgr.allocate_segments();

        // Report failure
        let reassign = mgr.report_segment_failed(0, 503);
        assert!(reassign.is_some());

        // Check that stats were updated
        let stat = stat_man.find_stat("failing.mirror.com").unwrap();
        assert_eq!(stat.get_consecutive_failures(), 1);
        assert_eq!(stat.get_last_error_code(), 503);
    }

    #[test]
    fn test_extract_host_from_url() {
        assert_eq!(
            extract_host_from_url("http://example.com/path"),
            "example.com"
        );
        assert_eq!(
            extract_host_from_url("https://host:8080/file?q=1"),
            "host:8080"
        );
        assert_eq!(extract_host_from_url("ftp://server.com"), "server.com");
        assert_eq!(extract_host_from_url("not-a-url"), "not-a-url");
    }

    #[test]
    fn test_get_mirror_url() {
        let mgr = ConcurrentSegmentManager::new(
            100,
            vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
            Some(100),
        );

        assert_eq!(mgr.get_mirror_url(0), Some("http://a.com/f"));
        assert_eq!(mgr.get_mirror_url(1), Some("http://b.com/f"));
        assert_eq!(mgr.get_mirror_url(999), None);
    }

    #[test]
    fn test_mirror_active_segments() {
        let mut mgr = ConcurrentSegmentManager::new(
            300,
            vec!["http://a.com/f".to_string()],
            Some(100),
        );

        assert_eq!(mgr.mirror_active_segments(0), 0);
        assert_eq!(mgr.num_segments(), 3);

        // Set max connections to allow all 3 segments
        mgr.set_max_connections_per_mirror(3);

        mgr.allocate_segments();
        // After allocation, all 3 segments should be assigned to the single mirror
        assert_eq!(mgr.mirror_active_segments(0), 3);
    }

    #[test]
    fn test_no_intelligent_selection_by_default() {
        let mgr = ConcurrentSegmentManager::new(
            100,
            vec!["http://a.com/f".to_string()],
            Some(100),
        );

        assert!(!mgr.has_intelligent_selection());
    }
}
