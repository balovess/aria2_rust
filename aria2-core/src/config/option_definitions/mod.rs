//! Built-in option definitions for aria2-rust.
//!
//! This module contains the registration of all built-in configuration
//! options, organized by category. Each category has its own registration
//! method on [`OptionRegistry`](super::OptionRegistry) for clear separation
//! of concerns.
//!
//! # Original CLI Contract
//!
//! Original registry short names are copied from the active option handlers
//! and usage definitions in `aria2_original`. They are public compatibility
//! data, not an internal prioritization scheme:
//!
//! `-a file-allocation`, `-c continue`, `-d dir`, `-D daemon`,
//! `-i input-file`, `-j max-concurrent-downloads`,
//! `-k min-split-size`, `-l log`, `-m max-tries`, `-M metalink-file`,
//! `-n no-netrc`, `-o out`, `-O index-out`, `-p ftp-pasv`,
//! `-P parameterized-uri`, `-q quiet`,
//! `-R remote-time`, `-s split`, `-S show-files`, `-t timeout`,
//! `-T torrent-file`, `-u max-upload-limit`, `-U user-agent`,
//! `-V check-integrity`, `-x max-connection-per-server`, and
//! `-Z force-sequential`.
//!
//! `-h/--help` and `-v/--version` are CLI actions rather than registry
//! options. The Rust CLI additionally accepts `-B`, `-e`, `-g`, `-G`, `-I`,
//! `-L`, `-r`, and `-X` as additive aliases for long options that have no
//! upstream short form. These aliases are Rust product extensions and are
//! tested separately from the original short-option contract.

mod advanced;
#[cfg(feature = "bittorrent")]
mod bittorrent;
mod general;
mod http_ftp;
mod rpc;

/// Extension trait that adds categorized registration methods to
/// [`OptionRegistry`](super::OptionRegistry).
///
/// This trait is implemented for [`super::OptionRegistry`] and provides one
/// method per option category, making it easy to register options in logical
/// groups or to selectively enable/disable categories.
#[allow(dead_code)] // Trait methods are called dynamically via impl blocks
pub(super) trait RegisterOptions {
    /// Register all General category options (directory, logging, UI, session).
    fn register_general_options(&mut self);

    /// Register all HTTP/FTP category options (proxies, headers, timeouts, connections).
    fn register_http_ftp_options(&mut self);

    /// Register all BitTorrent category options (seeding, DHT, PEX, peers).
    #[cfg(feature = "bittorrent")]
    fn register_bt_options(&mut self);

    /// Register all RPC category options (JSON-RPC/XML-RPC server settings).
    fn register_rpc_options(&mut self);

    /// Register all Advanced category options (bandwidth limits, disk cache, allocation).
    fn register_advanced_options(&mut self);

    /// Convenience method that registers all categories at once.
    fn register_all_options(&mut self) {
        self.register_general_options();
        self.register_http_ftp_options();
        #[cfg(feature = "bittorrent")]
        self.register_bt_options();
        self.register_rpc_options();
        self.register_advanced_options();
    }
}

// Note: The impl RegisterOptions for OptionRegistry block is in option.rs
// since OptionRegistry is defined there. The individual register_*_options
// methods are defined in the category sub-modules (general.rs, http_ftp.rs,
// etc.) as separate impl blocks.
