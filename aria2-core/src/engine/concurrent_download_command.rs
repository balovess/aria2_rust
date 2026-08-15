//! Metalink compatibility adapter for the unified HTTP download command.
//!
//! Metalink callers historically constructed `ConcurrentDownloadCommand`
//! directly. Keep that public seam, but route its HTTP work through
//! `DownloadCommand`, which owns the only production concurrent-download
//! pipeline and its adaptive authority executor.

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::command::{Command, CommandStatus};
use crate::engine::download_command::DownloadCommand;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

pub struct ConcurrentDownloadCommand {
    inner: DownloadCommand,
}

impl ConcurrentDownloadCommand {
    pub fn new(
        gid: GroupId,
        metalink_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(metalink_bytes, None)
            .map_err(|error| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Metalink parse failed: {error}"
            )))
        })?;
        let file = doc.files.first().ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config("Metalink contains no files".into()))
        })?;
        let urls: Vec<String> = file
            .get_sorted_urls()
            .iter()
            .map(|entry| entry.url.clone())
            .collect();
        let first_url = urls
            .first()
            .cloned()
            .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("No URLs in Metalink".into())))?;

        let mut effective_options = options.clone();
        if effective_options.checksum.is_none()
            && let Some(hash) = file.hashes.first()
        {
            effective_options.checksum =
                Some((hash.algo.as_standard_name().to_string(), hash.value.clone()));
        }

        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            urls,
            effective_options.clone(),
        )));
        if let Some(size) = file.size {
            group.recover().set_total_length(size);
        }
        group.recover().set_output_name(file.name.clone());

        let inner = DownloadCommand::new_with_group(
            group,
            &first_url,
            &effective_options,
            output_dir,
            Some(&file.name),
        )?;

        Ok(Self { inner })
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.inner.group()
    }
}

#[async_trait]
impl Command for ConcurrentDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        self.inner.execute().await
    }

    async fn shutdown(&mut self) {
        self.inner.shutdown().await;
    }

    fn status(&self) -> CommandStatus {
        self.inner.status()
    }

    fn gid(&self) -> GroupId {
        self.inner.gid()
    }

    fn request_group(&self) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        self.inner.request_group()
    }

    fn timeout(&self) -> Option<Duration> {
        self.inner.timeout()
    }
}
