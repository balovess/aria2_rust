use crate::constants;

/// Status of a single download segment.
#[derive(Debug, Clone, PartialEq)]
pub enum SegmentStatus {
    Pending,
    Downloading,
    Done,
    Failed,
}

/// A contiguous byte range within the total file being downloaded.
#[derive(Debug)]
pub struct Segment {
    pub index: u32,
    pub offset: u64,
    pub length: u64,
    pub status: SegmentStatus,
    pub assigned_mirror: Option<usize>,
    pub retry_count: u32,
}

impl Segment {
    pub(crate) fn new(index: u32, offset: u64, length: u64) -> Self {
        Self {
            index,
            offset,
            length,
            status: SegmentStatus::Pending,
            assigned_mirror: None,
            retry_count: 0,
        }
    }
}

/// Runtime state tracked per mirror URL.
#[derive(Debug)]
pub struct MirrorState {
    pub speed: f64,
    pub active_segments: usize,
    pub max_connections: usize,
    pub consecutive_failures: usize,
    pub disabled: bool,
}

impl MirrorState {
    pub(crate) fn new() -> Self {
        Self {
            speed: 0.0,
            active_segments: 0,
            max_connections: constants::DEFAULT_MAX_CONNECTIONS_PER_MIRROR,
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
