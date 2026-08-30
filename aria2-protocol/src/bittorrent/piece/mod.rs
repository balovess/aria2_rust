//! BitTorrent wire-level piece representations.
//!
//! Download scheduling, peer availability policy, completion state, and hash
//! verification live in `aria2-core::engine::bt_piece`. This module contains
//! only representations shared with the BitTorrent message layer.

pub mod bitfield;
