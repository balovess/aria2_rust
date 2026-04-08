use std::collections::VecDeque;

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
    pub data: Option<Vec<u8>>,
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
    segment_size: u64,
    segments: Vec<Segment>,
    mirrors: Vec<MirrorState>,
    completed_bytes: u64,
    max_retries_per_segment: u32,
    max_mirror_failures: usize,
}

impl ConcurrentSegmentManager {
    pub fn new(total_size: u64, urls: Vec<String>, segment_size: Option<u64>) -> Self {
        let seg_size = segment_size.unwrap_or(1_048_576);
        let num_segments = if total_size == 0 { 0 } else { ((total_size + seg_size - 1) / seg_size) as usize };

        let mut segments = Vec::with_capacity(num_segments);
        for i in 0..num_segments {
            let offset = (i as u64) * seg_size;
            let remaining = total_size.saturating_sub(offset);
            let length = seg_size.min(remaining);
            segments.push(Segment::new(i as u32, offset, length));
        }

        let mirrors = urls.into_iter().map(MirrorState::new).collect();

        Self {
            total_size,
            segment_size: seg_size,
            segments,
            mirrors,
            completed_bytes: 0,
            max_retries_per_segment: 3,
            max_mirror_failures: 3,
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
        self.segments.iter_mut()
            .find(|s| s.status == SegmentStatus::Pending)
    }

    pub fn next_pending_segment_for_mirror(&mut self, mirror_idx: usize) -> Option<(u32, u64, u64)> {
        if !self.mirrors.get(mirror_idx).map_or(false, |m| m.can_accept_more()) {
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

    pub fn complete_segment(&mut self, index: u32, data: Vec<u8>) -> bool {
        if let Some(seg) = self.segments.get_mut(index as usize) {
            seg.status = SegmentStatus::Done;
            seg.data = Some(data);

            if let Some(mi) = seg.assigned_mirror {
                if let Some(m) = self.mirrors.get_mut(mi) {
                    m.active_segments = m.active_segments.saturating_sub(1);
                    m.consecutive_failures = 0;
                }
            }

            self.completed_bytes += seg.length;
            true
        } else {
            false
        }
    }

    pub fn fail_segment(&mut self, index: u32) -> Option<usize> {
        let (prev_mirror, new_retry) = {
            if let Some(seg) = self.segments.get(index as usize) {
                (seg.assigned_mirror, seg.retry_count + 1)
            } else {
                return None;
            }
        };

        if let Some(mi) = prev_mirror {
            if let Some(m) = self.mirrors.get_mut(mi) {
                m.active_segments = m.active_segments.saturating_sub(1);
                m.consecutive_failures += 1;
                if m.consecutive_failures >= self.max_mirror_failures {
                    m.disabled = true;
                }
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
        self.mirrors.iter()
            .enumerate()
            .filter(|(i, m)| *i != exclude && m.is_available())
            .map(|(i, _)| i)
            .next()
    }

    pub fn is_complete(&self) -> bool {
        self.segments.iter().all(|s| s.status == SegmentStatus::Done)
    }

    pub fn has_failed_segments(&self) -> bool {
        self.segments.iter().any(|s| s.status == SegmentStatus::Failed)
    }

    pub fn has_pending_segments(&self) -> bool {
        self.segments.iter().any(|s| s.status == SegmentStatus::Pending)
    }

    pub fn assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() || self.total_size == 0 {
            return None;
        }

        let mut result = Vec::with_capacity(self.total_size as usize);
        for seg in &self.segments {
            if let Some(ref data) = seg.data {
                result.extend_from_slice(data);
            } else {
                return None;
            }
        }
        Some(result)
    }

    pub fn progress(&self) -> f64 {
        if self.total_size == 0 { return 100.0; }
        let done = self.segments.iter().filter(|s| s.status == SegmentStatus::Done).count();
        done as f64 / self.segments.len() as f64 * 100.0
    }

    pub fn num_segments(&self) -> usize { self.segments.len() }
    pub fn segment_status(&self, index: usize) -> Option<SegmentStatus> {
        self.segments.get(index).map(|s| s.status.clone())
    }
    pub fn num_mirrors(&self) -> usize { self.mirrors.len() }
    pub fn total_size(&self) -> u64 { self.total_size }
    pub fn completed_bytes(&self) -> u64 { self.completed_bytes }

    pub fn mirror_url(&self, index: usize) -> Option<&str> {
        self.mirrors.get(index).map(|m| m.url.as_str())
    }

    pub fn available_mirrors(&self) -> Vec<usize> {
        self.mirrors.iter()
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

    pub fn set_max_retries(&mut self, retries: u32) {
        self.max_retries_per_segment = retries;
    }

    pub fn segment_info(&self, index: usize) -> Option<(u64, u64, &SegmentStatus)> {
        self.segments.get(index).map(|s| (s.offset, s.length, &s.status))
    }
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
        let mgr = ConcurrentSegmentManager::new(3_000_000, vec![
            "http://a.com/f".to_string(),
            "http://b.com/f".to_string(),
        ], Some(1_000_000));
        assert_eq!(mgr.num_segments(), 3);
        assert_eq!(mgr.num_mirrors(), 2);
    }

    #[test]
    fn test_allocate_segments_round_robin() {
        let mut mgr = ConcurrentSegmentManager::new(3_000_000, vec![
            "http://a.com/f".to_string(),
            "http://b.com/f".to_string(),
        ], Some(1_000_000));

        mgr.allocate_segments();

        let assigned_a: Vec<_> = mgr.segments.iter()
            .filter(|s| s.assigned_mirror == Some(0))
            .map(|s| s.index)
            .collect();
        let assigned_b: Vec<_> = mgr.segments.iter()
            .filter(|s| s.assigned_mirror == Some(1))
            .map(|s| s.index)
            .collect();

        assert!(!assigned_a.is_empty());
        assert!(!assigned_b.is_empty());
        assert_eq!(assigned_a.len() + assigned_b.len(), 3);
    }

    #[test]
    fn test_complete_and_assemble() {
        let mut mgr = ConcurrentSegmentManager::new(200, vec!["http://x.com/f".to_string()], Some(100));

        mgr.allocate_segments();
        assert_eq!(mgr.progress(), 0.0);

        mgr.complete_segment(0, vec![0xAB; 100]);
        assert!(!mgr.is_complete());
        assert!((mgr.progress() - 50.0).abs() < 0.01);

        mgr.complete_segment(1, vec![0xCD; 100]);
        assert!(mgr.is_complete());
        assert!((mgr.progress() - 100.0).abs() < 0.01);

        let assembled = mgr.assemble().unwrap();
        assert_eq!(assembled.len(), 200);
        assert_eq!(&assembled[..100], &[0xAB; 100][..]);
        assert_eq!(&assembled[100..], &[0xCD; 100][..]);
    }

    #[test]
    fn test_fail_and_reassign() {
        let mut mgr = ConcurrentSegmentManager::new(200, vec![
            "http://a.com/f".to_string(),
            "http://b.com/f".to_string(),
        ], Some(100));

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
        let mut mgr = ConcurrentSegmentManager::new(100, vec![
            "http://a.com/f".to_string(),
        ], Some(100));
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
        let mut mgr = ConcurrentSegmentManager::new(300, vec![
            "http://a.com/f".to_string(),
            "http://b.com/f".to_string(),
        ], Some(100));

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
}
