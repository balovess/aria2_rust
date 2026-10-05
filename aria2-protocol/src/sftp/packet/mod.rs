//! SFTP packet types, wire encoding, and protocol constants.
//!
//! Implements the SSH File Transfer Protocol (IETF draft-ietf-secsh-filexfer-02 / v3-v6)
//! packet types with binary encode/decode for the russh channel transport.
//!
//! ## Wire Format
//!
//! ```text
//! +----------------+----------------+
//! | uint32 length  | payload bytes  |
//! +----------------+----------------+
//!
//! Payload layout depends on packet type:
//!   INIT:     type(1) + version(4)
//!   VERSION:  type(1) + version(4) + extensions...
//!   Request:  type(1) + request_id(4) + ...
//!   Response: type(1) + request_id(4) + ...
//! ```

mod attrs;
mod codec;
mod constants;
#[cfg(test)]
mod tests;
mod types;
mod wire;

pub use attrs::SftpFileAttrs;
pub use constants::*;
pub use types::{SftpNameEntry, SftpPacket};
