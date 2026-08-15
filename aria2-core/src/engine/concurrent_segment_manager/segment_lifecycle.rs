use super::ConcurrentSegmentManager;
use super::types::SegmentStatus;

impl ConcurrentSegmentManager {
    /// Restore segments represented by the control-file bitfield.
    ///
    /// The bit order is the same as the persisted control-file format: the
    /// most significant bit of the first byte represents segment zero. Only
    /// complete segments are restored; an in-progress byte range is left
    /// pending so the downloader can fetch it again safely.
    pub fn restore_completed_from_bitfield(&mut self, bitfield: &[u8]) -> u64 {
        for segment in &mut self.segments {
            let byte_index = segment.index as usize / 8;
            let bit_index = segment.index as usize % 8;
            let completed = bitfield
                .get(byte_index)
                .is_some_and(|byte| byte & (1 << (7 - bit_index)) != 0);

            if !completed || segment.status == SegmentStatus::Done {
                continue;
            }

            if let Some(mirror_idx) = segment.assigned_mirror.take()
                && let Some(mirror) = self.mirrors.get_mut(mirror_idx)
            {
                mirror.active_segments = mirror.active_segments.saturating_sub(1);
            }
            segment.status = SegmentStatus::Done;
            segment.retry_count = 0;
        }

        self.completed_bytes = self
            .segments
            .iter()
            .filter(|segment| segment.status == SegmentStatus::Done)
            .map(|segment| segment.length)
            .sum();
        self.completed_bytes
    }

    /// Restore the fully completed prefix represented by a byte count.
    ///
    /// This is the conservative fallback for a control file created with a
    /// different segment layout or for sequential progress that has no
    /// segment bitfield. Partial segments remain pending.
    pub fn restore_completed_prefix(&mut self, length: u64) -> u64 {
        for segment in &mut self.segments {
            if segment.offset.saturating_add(segment.length) > length
                || segment.status == SegmentStatus::Done
            {
                continue;
            }

            if let Some(mirror_idx) = segment.assigned_mirror.take()
                && let Some(mirror) = self.mirrors.get_mut(mirror_idx)
            {
                mirror.active_segments = mirror.active_segments.saturating_sub(1);
            }
            segment.status = SegmentStatus::Done;
            segment.retry_count = 0;
        }

        self.completed_bytes = self
            .segments
            .iter()
            .filter(|segment| segment.status == SegmentStatus::Done)
            .map(|segment| segment.length)
            .sum();
        self.completed_bytes
    }

    /// Return a capacity-limited segment to the pending queue without
    /// consuming its ordinary retry budget.
    pub fn requeue_segment(&mut self, index: u32) -> bool {
        let Some(seg) = self.segments.get_mut(index as usize) else {
            return false;
        };
        if !matches!(seg.status, SegmentStatus::Downloading) {
            return false;
        }

        if let Some(mirror_idx) = seg.assigned_mirror
            && let Some(mirror) = self.mirrors.get_mut(mirror_idx)
        {
            mirror.active_segments = mirror.active_segments.saturating_sub(1);
        }
        seg.status = SegmentStatus::Pending;
        seg.assigned_mirror = None;
        true
    }

    /// Mark a segment as successfully downloaded.
    ///
    /// Returns `true` if the segment existed, `false` otherwise.
    pub fn complete_segment(&mut self, index: u32, len: usize) -> bool {
        if let Some(seg) = self.segments.get_mut(index as usize) {
            if seg.status == SegmentStatus::Done || len as u64 != seg.length {
                return false;
            }
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

        if self.max_retries_per_segment != 0 && new_retry >= self.max_retries_per_segment {
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
