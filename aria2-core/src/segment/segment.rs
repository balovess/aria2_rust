use crate::error::{Aria2Error, Result};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SegmentStatus {
    Pending,
    InProgress,
    Completed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    index: usize,
    start: u64,
    end: u64,
    completed_length: u64,
    status: SegmentStatus,
}

impl Segment {
    pub fn new(index: usize, start: u64, end: u64) -> Self {
        Segment {
            index,
            start,
            end,
            completed_length: 0,
            status: SegmentStatus::Pending,
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn length(&self) -> u64 {
        self.end - self.start
    }

    pub fn completed_length(&self) -> u64 {
        self.completed_length
    }

    pub fn remaining(&self) -> u64 {
        self.length() - self.completed_length
    }

    pub fn status(&self) -> SegmentStatus {
        self.status
    }

    pub fn is_completed(&self) -> bool {
        self.status == SegmentStatus::Completed
    }

    pub fn is_in_progress(&self) -> bool {
        self.status == SegmentStatus::InProgress
    }

    pub fn is_pending(&self) -> bool {
        self.status == SegmentStatus::Pending
    }

    pub fn range(&self) -> Range<u64> {
        self.start..self.end
    }

    pub fn write_data(&mut self, offset: u64, length: u64) -> Result<()> {
        if offset < self.start || offset + length > self.end {
            return Err(Aria2Error::DownloadFailed(format!(
                "数据偏移超出分段范围: {}-{}",
                offset, self.end
            )));
        }

        self.completed_length += length;
        self.status = SegmentStatus::InProgress;

        if self.completed_length >= self.length() {
            self.status = SegmentStatus::Completed;
        }

        Ok(())
    }

    pub fn set_status(&mut self, status: SegmentStatus) {
        self.status = status;
    }
}
