//! FTP negotiation module
//!
//! Implements the full FTP negotiation flow matching the C++ aria2
//! `FtpNegotiationCommand` state machine, but using Rust's async/await
//! model instead of 30+ explicit states.
//!
//! The negotiation flow (in order):
//! 1. Connect + read greeting (or skip if using pooled connection)
//! 2. Authenticate (or skip if pooled)
//! 3. FEAT - query server capabilities
//! 4. OPTS UTF8 ON (if FEAT reports UTF8 support)
//! 5. SYST - query server system type (for VMS path handling)
//! 6. Set TYPE (binary or ASCII, per config)
//! 7. PWD to get baseWorkingDir
//! 8. CWD traversal (split path, CWD each directory component)
//! 9. MDTM (if remote-time option is enabled)
//! 10. SIZE
//! 11. Choose data connection mode (EPSV/PASV or EPRT/PORT)
//!     - If proxy is configured for PASV, tunnel the data connection
//! 12. REST (after data connection established, per C++ ordering)
//!     - Verify data connection is alive before REST (C++ sendRestPasv)
//! 13. RETR
//!
//! After data transfer completes, call `finish_download()` to read the
//! 226 transfer-complete response and optionally pool the connection.
//!
//! # Module organization
//!
//! - [`control`]       - I/O layer: `RawFtpControl`, `FreshControl`, `PooledControl`
//! - [`capabilities`]  - FEAT parsing, server capability tracking, and new commands
//! - [`parsing`]       - Stateless response and path parsers
//! - [`fresh_commands`] - Fresh-control command helpers
//! - [`pooled_commands`] - Pooled-control command helpers
//! - [`fresh_flow`]    - FtpNegotiator methods for fresh (non-pooled) connections
//! - [`pooled_flow`]   - FtpNegotiator methods for pooled (pre-authenticated) connections
//! - [`orchestration`] - Public negotiation entry points and data-channel orchestration
//! - [`types`]         - Public configuration and result types

mod capabilities;
mod control;
mod fresh_commands;
mod fresh_flow;
mod orchestration;
mod parsing;
mod pooled_commands;
mod pooled_flow;

#[cfg(test)]
mod tests;

mod types;

pub(super) use crate::ftp::connection::active_data_bind_addr;
pub use capabilities::ServerCapabilities;
pub use control::RawFtpControl;
pub(crate) use control::read_response_impl;
pub(crate) use parsing::{
    cwd_targets, parse_epsv_response, parse_mdtm_timestamp, parse_pasv_response,
    parse_pwd_response, percent_decode, split_decoded_remote_path,
};
pub use types::{
    FtpDataProxyConfig, FtpNegotiationConfig, FtpNegotiationResult, FtpNegotiator, FtpTransferType,
};
