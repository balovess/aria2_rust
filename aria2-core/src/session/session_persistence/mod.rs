//! Session Save/Load Persistence - Phase 15 H4
//!
//! Provides complete session state persistence using the ResumeData JSON format
//! (.aria2 files). This module bridges the ActiveSessionManager with the
//! ResumeData serialization system for cross-restart download resumption.
//!
//! # Architecture
//!
//! ```text
//! session_persistence/
//!   ├── mod.rs           - Module declarations and public re-exports
//!   ├── persistence.rs   - SessionPersistence struct and core save/load logic
//!   ├── dht.rs           - DHT state snapshot types for persistence
//!   └── tests.rs         - Integration and unit tests
//!
//! Dependencies:
//!   resume_data.rs - ResumeData, UriState, ChecksumInfo structs
//!   active_session.rs - ActiveSessionManager for session file I/O
//! ```

mod dht;
mod persistence;

#[cfg(test)]
mod tests;

pub use dht::{DhtNodeInfo, DhtStateSnapshot};
pub use persistence::{SessionPersistence, DEFAULT_AUTO_SAVE_INTERVAL_SECS};
