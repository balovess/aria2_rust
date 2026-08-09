//! General category options: directory, logging, UI, session, download behavior.
//!
//! This module is organized into sub-modules by logical grouping:
//!
//! - [`basic`] — Directory, output, config files, session, daemon, GID, netrc
//! - [`logging`] — Log file, log level, progress interval
//! - [`ui`] — Quiet mode, color output, download result display, console readout
//! - [`download`] — Resume, integrity, concurrency, limits, event hooks, URI selection,
//!   mmap, checksum, resource limits
//! - [`network`] — Interface binding, DNS, event poll, server statistics
//! - [`metalink`] — Metalink preferences, torrent/metalink file input, show-files

mod basic;
mod download;
mod logging;
mod metalink;
mod network;
mod ui;

use crate::config::OptionRegistry;

impl OptionRegistry {
    /// Register general-purpose options: directory, output, logging, UI, session management.
    ///
    /// This is the top-level entry point that delegates to each sub-module's
    /// registration method, preserving the exact same option set as the original
    /// monolithic implementation.
    pub fn register_general_options(&mut self) {
        self.register_general_basic_options();
        self.register_general_logging_options();
        self.register_general_ui_options();
        self.register_general_download_options();
        self.register_general_network_options();
        self.register_general_metalink_options();
    }
}
