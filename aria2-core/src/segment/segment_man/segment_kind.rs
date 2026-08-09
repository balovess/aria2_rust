//! Enum dispatch for segment types, replacing C++ virtual dispatch.

use crate::segment::grow_segment::GrowSegment;
use crate::segment::piece::Piece;
use crate::segment::pieced_segment::PiecedSegment;

/// Enum dispatch for segment types, replacing C++ virtual dispatch.
///
/// The C++ implementation uses a `Segment` base class with virtual methods.
/// This Rust version uses an enum for zero-overhead dispatch and exhaustive
/// pattern matching.
///
/// # Variants
///
/// - [`Pieced`](SegmentKind::Pieced) — Fixed-length piece, wraps a `Piece`
/// - [`Grow`](SegmentKind::Grow) — Unknown-length download (chunked transfer)
#[derive(Debug)]
pub enum SegmentKind {
    /// Fixed-length piece segment (total length known)
    Pieced(Box<PiecedSegment>),
    /// Growing segment (total length unknown, e.g. chunked transfer)
    Grow(GrowSegment),
}

impl SegmentKind {
    /// Returns `true` if this segment is fully downloaded.
    pub fn is_complete(&self) -> bool {
        match self {
            SegmentKind::Pieced(p) => p.is_complete(),
            SegmentKind::Grow(g) => g.is_complete(),
        }
    }

    /// Returns the piece index.
    pub fn index(&self) -> usize {
        match self {
            SegmentKind::Pieced(p) => p.index(),
            SegmentKind::Grow(g) => g.index(),
        }
    }

    /// Returns the byte offset of this segment in the file.
    pub fn position(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.position(),
            SegmentKind::Grow(g) => g.position(),
        }
    }

    /// Returns the next byte position to write to.
    pub fn position_to_write(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.position_to_write(),
            SegmentKind::Grow(g) => g.position_to_write(),
        }
    }

    /// Returns the actual length of this segment.
    pub fn length(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.length(),
            SegmentKind::Grow(g) => g.length(),
        }
    }

    /// Returns the nominal segment/piece length.
    pub fn segment_length(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.segment_length(),
            SegmentKind::Grow(g) => g.segment_length(),
        }
    }

    /// Returns how many bytes have been written so far.
    pub fn written_length(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.written_length(),
            SegmentKind::Grow(g) => g.written_length(),
        }
    }

    /// Increments the written length by `bytes`.
    pub fn update_written_length(&mut self, bytes: u64) {
        match self {
            SegmentKind::Pieced(p) => p.update_written_length(bytes),
            SegmentKind::Grow(g) => g.update_written_length(bytes),
        }
    }

    /// Updates the hash computation with data at the given offset.
    pub fn update_hash(&mut self, begin: u64, data: &[u8]) -> bool {
        match self {
            SegmentKind::Pieced(p) => p.update_hash(begin, data),
            SegmentKind::Grow(g) => g.update_hash(begin, data),
        }
    }

    /// Returns `true` if the hash has been fully computed.
    pub fn is_hash_calculated(&self) -> bool {
        match self {
            SegmentKind::Pieced(p) => p.is_hash_calculated(),
            SegmentKind::Grow(g) => g.is_hash_calculated(),
        }
    }

    /// Returns the hash digest as a hex string, or empty if unavailable.
    pub fn digest(&mut self) -> String {
        match self {
            SegmentKind::Pieced(p) => p.digest(),
            SegmentKind::Grow(g) => g.digest(),
        }
    }

    /// Returns a reference to the underlying piece, if any.
    ///
    /// Returns `None` for grow segments.
    pub fn piece(&self) -> Option<&Piece> {
        match self {
            SegmentKind::Pieced(p) => Some(p.piece()),
            SegmentKind::Grow(_) => None,
        }
    }

    /// Returns a mutable reference to the underlying piece, if any.
    ///
    /// Returns `None` for grow segments.
    pub fn piece_mut(&mut self) -> Option<&mut Piece> {
        match self {
            SegmentKind::Pieced(p) => Some(p.piece_mut()),
            SegmentKind::Grow(_) => None,
        }
    }
}

impl PartialEq for SegmentKind {
    fn eq(&self, other: &Self) -> bool {
        self.index() == other.index()
    }
}

impl Eq for SegmentKind {}
