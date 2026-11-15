//! Built-in option definitions for aria2-rust.
//!
//! This module contains the registration of all built-in configuration
//! options, organized by category. Each category has its own registration
//! method on [`OptionRegistry`](super::OptionRegistry) for clear separation
//! of concerns.
//!
//! # Original CLI Contract
//!
//! The registry keeps a Rust-owned baseline for the original public short
//! options. They are compatibility data, not an internal prioritization
//! scheme, and are intentionally independent of the Rust field layout:
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
mod sftp;
