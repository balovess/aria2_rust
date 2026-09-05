use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::debug;

use crate::constants;
use crate::selector::server_stat_man::ServerStatMan;
use crate::selector::uri_selector::UriSelector;

// ── Sub-modules ─────────────────────────────────────────────────────────
mod mirror_selection;
mod segment_lifecycle;
#[cfg(test)]
mod tests;
mod types;

// ── Public re-exports ───────────────────────────────────────────────────
pub use types::{MirrorState, Segment, SegmentStatus};

// ── Main struct ─────────────────────────────────────────────────────────

pub struct ConcurrentSegmentManager {
    total_size: u64,
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
    /// Atomic hint pointing at the next candidate segment index for allocation.
    ///
    /// This enables O(1) amortized segment allocation: scans start from the last
    /// assigned position instead of re-scanning already-assigned segments from
    /// the beginning. Updated with `Ordering::Relaxed` since it is only a hint;
    /// the linear scan with wraparound always preserves correctness even if the
    /// hint races or points at an already-assigned segment.
    next_segment_idx: AtomicU32,
}

impl ConcurrentSegmentManager {
    /// Create a new segment manager with basic round-robin mirror selection.
    ///
    /// This is the basic constructor that uses the built-in `MirrorState`
    /// for mirror tracking. For intelligent mirror selection, use
    /// `new_with_selector()` instead.
    pub fn new(total_size: u64, urls: Vec<String>, segment_size: Option<u64>) -> Self {
        let seg_size = segment_size.unwrap_or(constants::DEFAULT_SEGMENT_SIZE as u64);
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

        let mirrors = urls.iter().map(|_| MirrorState::new()).collect();

        Self {
            total_size,
            segments,
            mirrors,
            mirror_urls: urls,
            completed_bytes: 0,
            max_retries_per_segment: constants::MAX_RETRIES_PER_SEGMENT,
            max_mirror_failures: constants::MAX_MIRROR_FAILURES as usize,
            stat_man: None,
            uri_selector: None,
            next_segment_idx: AtomicU32::new(0),
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
        let seg_size = segment_size.unwrap_or(constants::DEFAULT_SEGMENT_SIZE as u64);
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

        let mirrors = urls.iter().map(|_| MirrorState::new()).collect();

        Self {
            total_size,
            segments,
            mirrors,
            mirror_urls: urls,
            completed_bytes: 0,
            max_retries_per_segment: constants::MAX_RETRIES_PER_SEGMENT,
            max_mirror_failures: constants::MAX_MIRROR_FAILURES as usize,
            stat_man: Some(stat_man),
            uri_selector: Some(uri_selector),
            next_segment_idx: AtomicU32::new(0),
        }
    }

    // ── Segment allocation ──────────────────────────────────────────────

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
        let len = self.segments.len();
        if len == 0 {
            return None;
        }
        // Start scanning from the atomic hint to skip already-assigned segments.
        // The wraparound scan guarantees correctness even if the hint is stale.
        let start = (self.next_segment_idx.load(Ordering::Relaxed) as usize) % len;
        for i in 0..len {
            let idx = (start + i) % len;
            if self.segments[idx].status == SegmentStatus::Pending {
                // Advance the hint past this segment; the caller marks it
                // Downloading immediately so it won't be selected again.
                self.next_segment_idx
                    .store((idx + 1) as u32, Ordering::Relaxed);
                return Some(&mut self.segments[idx]);
            }
        }
        None
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

        let len = self.segments.len();
        if len == 0 {
            return None;
        }
        // Start scanning from the atomic hint to skip already-assigned segments.
        // This yields O(1) amortized allocation: each segment is visited at most
        // once before the hint catches up to it.
        let start = (self.next_segment_idx.load(Ordering::Relaxed) as usize) % len;
        for i in 0..len {
            let idx = (start + i) % len;
            let seg = &mut self.segments[idx];
            if seg.status == SegmentStatus::Pending {
                seg.status = SegmentStatus::Downloading;
                seg.assigned_mirror = Some(mirror_idx);
                self.next_segment_idx
                    .store((idx + 1) as u32, Ordering::Relaxed);
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

    /// Atomically allocate the next segment index for lock-free assignment.
    ///
    /// Returns the index of the next segment to claim, or `None` if all segments
    /// have been allocated. This uses `fetch_add` for O(1) lock-free allocation.
    /// The caller is responsible for checking the segment status and claiming it.
    ///
    /// Unlike [`next_pending_segment_for_mirror`](Self::next_pending_segment_for_mirror),
    /// this method takes only `&self` and is therefore safe to call concurrently
    /// from multiple threads through an `Arc<ConcurrentSegmentManager>`.
    ///
    /// # Concurrency
    ///
    /// Each successful call returns a distinct index. Once all segments have been
    /// issued, subsequent calls return `None` (the counter keeps growing but is
    /// bounded by the number of callers, so it cannot overflow in practice).
    pub fn allocate_next_index(&self) -> Option<u32> {
        let idx = self.next_segment_idx.fetch_add(1, Ordering::Relaxed);
        if (idx as usize) < self.segments.len() {
            Some(idx)
        } else {
            None
        }
    }

    /// Reset the allocation index to 0.
    ///
    /// This is useful for retry scenarios where segments that previously failed
    /// need to be reconsidered for assignment from the beginning of the vector.
    ///
    /// # Safety of use
    ///
    /// This must not be called concurrently with [`allocate_next_index`](Self::allocate_next_index)
    /// if unique indices are required; it is intended for reset points where the
    /// caller has exclusive access (e.g. between download attempts).
    pub fn reset_allocation_index(&self) {
        self.next_segment_idx.store(0, Ordering::Relaxed);
    }

    // ── Query / inspection ──────────────────────────────────────────────

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

    pub fn completed_ranges(&self) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        for seg in &self.segments {
            if seg.status == SegmentStatus::Done {
                ranges.push((seg.offset, seg.length));
            }
        }
        ranges.sort_by_key(|r| r.0);
        ranges
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
        self.mirror_urls.get(index).map(String::as_str)
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

    pub fn segment_retry_count(&self, seg_idx: u32) -> u32 {
        self.segments
            .iter()
            .find(|s| s.index == seg_idx)
            .map(|s| s.retry_count)
            .unwrap_or(0)
    }

    pub fn has_permanently_failed_segments(&self) -> bool {
        self.segments.iter().any(|s| {
            s.status == SegmentStatus::Failed
                && self.max_retries_per_segment != 0
                && s.retry_count >= self.max_retries_per_segment
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
                        "Segment {} partially completed: {}/{} bytes",
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

    // ── Mirror configuration ────────────────────────────────────────────

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
}
