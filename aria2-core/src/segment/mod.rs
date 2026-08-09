#[cfg(feature = "bittorrent")]
pub mod bitfield;
pub mod bitfield_util;
pub mod grow_segment;
pub mod piece;
#[cfg(feature = "bittorrent")]
pub mod piece_selector;
#[cfg(feature = "bittorrent")]
pub mod piece_stat_man;
pub mod piece_storage;
pub mod pieced_segment;
pub mod segment_man;
pub mod unknown_length_piece_storage;
// Re-export the segment submodule with the same name as parent module
// This is intentional for API consistency
#[allow(clippy::module_inception)]
pub mod segment;

#[cfg(feature = "bittorrent")]
pub use bitfield::{Bitfield, test_bit};
pub use grow_segment::GrowSegment;
pub use piece::Piece;
#[cfg(feature = "bittorrent")]
pub use piece_selector::{PieceSelectorKind, PriorityPieceSelector, RarestPieceSelector};
#[cfg(feature = "bittorrent")]
pub use piece_stat_man::PieceStatMan;
pub use piece_storage::{BitfieldMan, DefaultPieceStorage, PieceStorage, StreamPieceSelectorKind};
pub use pieced_segment::PiecedSegment;
pub use segment::Segment;
pub use segment_man::{PeerStat, PeerStatus, SegmentKind, SegmentMan};
pub use unknown_length_piece_storage::UnknownLengthPieceStorage;
