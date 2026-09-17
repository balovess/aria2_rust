//! FTP download command module.
//!
//! Handles the complete FTP download lifecycle: URI parsing, control channel
//! management, data transfer, and retry logic.

mod attempt;
mod control;
mod execution;
mod proxy;
#[cfg(test)]
mod tests;
mod transfer;
mod types;

pub use types::FtpDownloadCommand;
