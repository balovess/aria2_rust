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
            self.completed_lengths[segment.index as usize] = segment.length;
        }

        self.completed_bytes = self
            .segments
            .iter()
            .filter(|segment| segment.status == SegmentStatus::Done)
            .map(|segment| segment.length)
            .sum();
        self.completed_bytes
    }

    /// Restore a verified contiguous prefix represented by a byte count.
    ///
    /// This is the conservative fallback for a control file created with a
    /// different segment layout or for sequential progress that has no
    /// segment bitfield. A partial parent stays pending and resumes at its
    /// first missing byte for this process; it is not persisted as complete.
    pub fn restore_completed_prefix(&mut self, length: u64) -> u64 {
        let length = length.min(self.total_size);
        for (index, segment) in self.segments.iter_mut().enumerate() {
            let completed = length.saturating_sub(segment.offset).min(segment.length);
            self.completed_lengths[index] = completed;
            if completed == segment.length {
                if let Some(mirror_idx) = segment.assigned_mirror.take()
                    && let Some(mirror) = self.mirrors.get_mut(mirror_idx)
                {
                    mirror.active_segments = mirror.active_segments.saturating_sub(1);
                }
                segment.status = SegmentStatus::Done;
                segment.retry_count = 0;
            } else {
                if let Some(mirror_idx) = segment.assigned_mirror.take()
                    && let Some(mirror) = self.mirrors.get_mut(mirror_idx)
                {
                    mirror.active_segments = mirror.active_segments.saturating_sub(1);
                }
                segment.status = SegmentStatus::Pending;
            }
        }
        self.completed_bytes = length;
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
        self.complete_range(index, len as u64) == Some(true)
    }

    /// Credit one successfully downloaded subrange within a durable parent.
    ///
    /// Returns `Some(true)` when the parent is now complete, `Some(false)`
    /// when more bytes remain, and `None` when the result is stale or invalid.
    pub fn complete_range(&mut self, index: u32, len: u64) -> Option<bool> {
        let segment_index = index as usize;
        let segment = self.segments.get_mut(segment_index)?;
        if segment.status != SegmentStatus::Downloading || len == 0 {
            return None;
        }

        let completed = self.completed_lengths.get_mut(segment_index)?;
        let remaining = segment.length.saturating_sub(*completed);
        if len > remaining {
            return None;
        }
        *completed += len;
        self.completed_bytes = self.completed_bytes.saturating_add(len);
        let parent_complete = *completed == segment.length;
        segment.status = if parent_complete {
            SegmentStatus::Done
        } else {
            SegmentStatus::Pending
        };

        if let Some(mirror_idx) = segment.assigned_mirror.take()
            && let Some(mirror) = self.mirrors.get_mut(mirror_idx)
        {
            mirror.active_segments = mirror.active_segments.saturating_sub(1);
            mirror.consecutive_failures = 0;
        }

        Some(parent_complete)
    }

    fn mark_mirror_failed(&mut self, mirror_idx: Option<usize>) {
        if let Some(mi) = mirror_idx
            && let Some(mirror) = self.mirrors.get_mut(mi)
        {
            mirror.active_segments = mirror.active_segments.saturating_sub(1);
            mirror.consecutive_failures += 1;
            if mirror.consecutive_failures >= self.max_mirror_failures {
                mirror.disabled = true;
            }
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

        self.mark_mirror_failed(prev_mirror);

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

    /// Stop retrying the failed mirror and move the segment to another mirror.
    ///
    /// Terminal HTTP responses must not consume the retry budget for the
    /// replacement mirror. This mirrors aria2's behavior of aborting the
    /// current URI and creating the next request from the remaining URI pool.
    pub fn fail_segment_without_retry(&mut self, index: u32) -> Option<usize> {
        let prev_mirror = self
            .segments
            .get(index as usize)
            .and_then(|segment| segment.assigned_mirror);
        self.mark_mirror_failed(prev_mirror);

        let reassign = self.find_available_mirror_for_reassignment(prev_mirror.unwrap_or(0));
        if let Some(segment) = self.segments.get_mut(index as usize) {
            segment.status = SegmentStatus::Pending;
            segment.assigned_mirror = reassign;
            segment.retry_count = 0;
        }
        reassign
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
