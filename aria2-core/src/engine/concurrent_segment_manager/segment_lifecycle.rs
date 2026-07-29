use super::types::SegmentStatus;
use super::ConcurrentSegmentManager;

impl ConcurrentSegmentManager {
    /// Mark a segment as successfully downloaded.
    ///
    /// Returns `true` if the segment existed, `false` otherwise.
    pub fn complete_segment(&mut self, index: u32, _len: usize) -> bool {
        if let Some(seg) = self.segments.get_mut(index as usize) {
            seg.status = SegmentStatus::Done;

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

    /// Mark a segment as failed and attempt reassignment to another mirror.
    ///
    /// Returns `Some(new_mirror_idx)` if the segment was reassigned, or `None`
    /// if the segment has permanently failed (max retries exhausted) or no
    /// alternative mirror is available.
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

    /// Find the first available mirror that is not `exclude`.
    fn find_available_mirror_for_reassignment(&self, exclude: usize) -> Option<usize> {
        self.mirrors
            .iter()
            .enumerate()
            .filter(|(i, m)| *i != exclude && m.is_available())
            .map(|(i, _)| i)
            .next()
    }
}
