//! Netrc file parser for ~/.netrc credentials.
//!
//! Parses the standard `.netrc` file format used by FTP/HTTP clients to store
//! credentials per-machine. This is a Rust port of the C++ `Netrc` class from
//! aria2, using the same state-machine parsing approach.
//!
//! # Supported tokens
//!
//! - `machine <name>` — identify a remote machine
//! - `login <name>` — user name for the machine
//! - `password <string>` — password for the machine
//! - `account <string>` — account (rarely used)
//! - `default` — matches any host without an explicit entry (must come last)
//! - `macdef <name>` — macro definition (skipped, not needed by aria2)
//!
//! # Format rules
//!
//! - Lines starting with `#` are comments
//! - Tokens are whitespace-separated
//! - Keywords are case-insensitive (machine/MACHINE both valid)
//! - `default` entry is optional and must come last
//!
//! # Example
//!
//! ```rust,ignore
//! use aria2_core::http::auth::netrc::NetrcParser;
//!
//! let content = "machine ftp.example.com\nlogin myuser\npassword mypass\n";
//! let parser = NetrcParser::parse(content).unwrap();
//! let entry = parser.find("ftp.example.com");
//! assert!(entry.is_some());
//! ```

mod error;
mod lookup;
mod parser;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public items so they remain accessible at the same paths.
pub use error::NetrcError;
pub use lookup::find_netrc_file;
pub use parser::NetrcParser;
pub use types::NetrcEntry;
