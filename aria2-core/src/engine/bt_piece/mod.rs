//! BitTorrent piece state and scheduling policy.
//!
//! This module owns task-facing piece state: selection, availability tracking,
//! completion accounting, and piece hash verification. Wire representations
//! such as [`Bitfield`] remain in `aria2-protocol`.
//!
//! The module is public for advanced integrations, but the preferred library
//! interface is the root-level re-exports from this crate.

pub mod manager;
pub mod peer_tracker;
pub mod picker;

pub use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
pub use manager::PieceManager;
pub use peer_tracker::{PeerBitfieldEntry, PeerBitfieldTracker, PeerTrackerStats};
pub use picker::{
    PickedPiece, PieceInfo, PiecePickStrategy, PiecePicker, PiecePickerConfig, PiecePriorityMode,
    PieceSelectionStrategy,
};
