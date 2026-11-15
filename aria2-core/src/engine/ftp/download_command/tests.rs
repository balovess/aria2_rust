//! Tests for FTP download command.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::control::RawFtpControl;
use super::types::FtpDownloadCommand;
use crate::engine::command::Command;
use crate::error::Aria2Error;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::ftp::connection::{
    parse_epsv_response, parse_pasv_response, parse_size_response as parse_ftp_size_response,
    percent_decode as urlencoding_decode,
};

struct SharedBufferWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBufferWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("test log buffer poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

mod configuration;
mod control_channel;
mod lifecycle;
mod parsing;
mod timeouts;
mod transfer;
