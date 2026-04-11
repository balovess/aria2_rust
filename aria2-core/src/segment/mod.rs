pub mod bitfield;
pub mod pieced_segment;
// Re-export the segment submodule with the same name as parent module
// This is intentional for API consistency
#[allow(clippy::module_inception)]
pub mod segment;

pub use bitfield::Bitfield;
pub use pieced_segment::PiecedSegment;
pub use segment::Segment;
