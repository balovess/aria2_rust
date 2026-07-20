#[cfg(feature = "bittorrent")]
pub mod bitfield;
#[cfg(feature = "bittorrent")]
pub mod pieced_segment;
// Re-export the segment submodule with the same name as parent module
// This is intentional for API consistency
#[allow(clippy::module_inception)]
pub mod segment;

#[cfg(feature = "bittorrent")]
pub use bitfield::Bitfield;
#[cfg(feature = "bittorrent")]
pub use pieced_segment::PiecedSegment;
pub use segment::Segment;
