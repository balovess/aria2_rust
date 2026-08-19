//! SessionPersistence struct and core save/load logic.
//!
//! This module is organized into sub-files by logical responsibility:
//!
//! - `types` - SessionPersistence struct, constants, constructors, and accessors
//! - `save_load` - Core save_state/load_state, restore_command, and auto-save
//! - `options` - Global session options persistence and cleanup
//! - `server_stats` - Server statistics save/load
//! - `selective_save` - Selective save (active-only, completed-only)
//! - `cookie` - Cookie jar persistence helpers

mod cookie;
mod options;
mod save_load;
mod selective_save;
mod server_stats;
mod types;

pub use types::{DEFAULT_AUTO_SAVE_INTERVAL_SECS, SessionPersistence};
