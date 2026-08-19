use super::ConcurrentSegmentManager;
use super::types::SegmentStatus;
use crate::selector::feedback_uri_selector::extract_host_and_protocol;

impl ConcurrentSegmentManager {
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
        self.select_mirror_for_next_segment_excluding(&[])
    }

    /// Select the next segment while skipping mirrors whose server is
    /// temporarily unavailable to the HTTP admission controller.
    pub fn select_mirror_for_next_segment_excluding(
        &mut self,
        excluded_mirrors: &[usize],
    ) -> Option<(usize, (u32, u64, u64))> {
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
            if let Some(mirror_idx) = selector.select(&self.mirror_urls, &used_hosts)
                && !excluded_mirrors.contains(&mirror_idx)
            {
                // Check if mirror can accept more
                if self
                    .mirrors
                    .get(mirror_idx)
                    .is_some_and(|m| m.can_accept_more())
                {
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
            if !excluded_mirrors.contains(&mirror_idx)
                && self.mirrors[mirror_idx].can_accept_more()
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
    /// * `data` - Downloaded data (length in bytes)
    /// * `bytes_per_sec` - Measured download speed in bytes per second
    /// * `is_multi_connection` - Whether this was a multi-connection download
    ///
    /// # Returns
    ///
    /// `true` if the segment was successfully marked as complete
    pub fn report_segment_complete(
        &mut self,
        seg_idx: u32,
        len: usize,
        bytes_per_sec: u64,
        is_multi_connection: bool,
    ) -> bool {
        // Get the mirror index before completing
        let mirror_idx = self
            .segments
            .get(seg_idx as usize)
            .and_then(|s| s.assigned_mirror);

        // Complete the segment
        let success = self.complete_segment(seg_idx, len);

        // Update server stats if available
        if success {
            if let (Some(idx), Some(stat_man)) = (mirror_idx, &self.stat_man)
                && let Some(url) = self.mirror_urls.get(idx)
                && let Some((host, protocol)) = extract_host_and_protocol(url)
            {
                stat_man.update_with_protocol(&host, &protocol, bytes_per_sec, is_multi_connection);

                // Reset failure count on success
                if let Some(stat) = stat_man.find_stat_by_protocol(&host, &protocol) {
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
            && let Some((host, protocol)) = extract_host_and_protocol(url)
        {
            // Ensure stat exists before marking failure
            stat_man.get_or_create_with_protocol(&host, &protocol);
            stat_man.mark_failure_with_protocol(&host, &protocol, error_code);

            // Check if mirror should be disabled
            if let Some(stat) = stat_man.find_stat_by_protocol(&host, &protocol)
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
pub(crate) fn extract_host_from_url(url: &str) -> String {
    extract_host_and_protocol(url)
        .map(|(host, _)| host)
        .unwrap_or_else(|| url.trim().to_string())
}
