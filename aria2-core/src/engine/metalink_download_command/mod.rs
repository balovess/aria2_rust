mod execution;
#[cfg(test)]
mod tests;
mod types;

pub use types::{select_mirrors_by_priority, try_mirrors_with_failover};

use std::sync::Arc;
use std::time::Duration;

use crate::error::{Aria2Error, FatalError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use types::FileDownloadInfo;

/// Information about a single file download created from a multi-file Metalink.
///
/// Returned by [`MetalinkDownloadCommand::create_multi_file`] so the caller
/// can track each per-file command independently.
pub struct MetalinkFileInfo {
    /// The download command for this file.
    pub command: MetalinkDownloadCommand,
    /// The original file index in the Metalink document (0-based).
    pub file_index: usize,
}

pub struct MetalinkDownloadCommand {
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    pub(crate) client: reqwest::Client,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    pub(crate) completed: bool,
    pub(crate) completed_bytes: u64,
    /// Raw Metalink data for re-parsing during execute().
    /// Only used for single-file mode. Empty in multi-file mode
    /// (each per-file command stores only its own file's data).
    pub(crate) metalink_data: Vec<u8>,
    /// Parsed file info for per-file mode (set by create_multi_file).
    /// When present, execute() uses this instead of re-parsing metalink_data.
    pub(crate) file_info: Option<FileDownloadInfo>,
    /// Parsed files for a grouped Metalink payload. A single command owns
    /// the group so the request context can schedule all selected files.
    pub(crate) grouped_file_infos: Vec<(std::path::PathBuf, FileDownloadInfo)>,
    pub(crate) checkpoint: Option<crate::engine::progress_checkpoint::ProgressCheckpoint>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` for mirror downloads.
    pub(crate) global_limiter: Option<RateLimiter>,
    #[cfg(feature = "bittorrent")]
    pub(crate) public_tracker_catalog:
        Option<Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    #[cfg(feature = "bittorrent")]
    pub(crate) bt_registry: Option<Arc<std::sync::RwLock<crate::engine::bt_registry::BtRegistry>>>,
    #[cfg(feature = "bittorrent")]
    pub(crate) bt_listener: Option<Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>>,
    #[cfg(feature = "bittorrent")]
    pub(crate) lpd_manager: Option<Arc<crate::engine::lpd_manager::LpdManager>>,
}

mod constructors;

impl MetalinkDownloadCommand {
    /// Get the output path for this download.
    pub fn output_path(&self) -> &std::path::Path {
        &self.output_path
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, mirror downloads acquire tokens from this limiter in addition
    /// to the per-download limiter.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_public_tracker_catalog(
        &mut self,
        catalog: Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>,
    ) {
        self.public_tracker_catalog = Some(catalog);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_registry(
        &mut self,
        registry: Arc<std::sync::RwLock<crate::engine::bt_registry::BtRegistry>>,
    ) {
        self.bt_registry = Some(registry);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_listener(
        &mut self,
        listener: Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>,
    ) {
        self.bt_listener = Some(listener);
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_lpd_manager(&mut self, manager: Arc<crate::engine::lpd_manager::LpdManager>) {
        self.lpd_manager = Some(manager);
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    /// Consume this command and return the inner `RequestGroup` Arc.
    ///
    /// Used by post-download handlers that need to extract the group
    /// for insertion into the reserved queue without cloning.
    pub fn into_group(self) -> Arc<std::sync::RwLock<RequestGroup>> {
        self.group
    }
}

/// Build the shared HTTP client for Metalink downloads.
pub(crate) fn build_http_client(options: &DownloadOptions) -> Result<reqwest::Client> {
    crate::http::client_pool::ensure_rustls_provider();
    let client_tls = crate::http::client_identity::ClientTlsConfig::from_download_options(options);
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .gzip(options.http_accept_gzip)
        .user_agent(crate::constants::USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(5));
    crate::http::client_identity::apply(builder, &client_tls)?
        .build()
        .map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "HTTP client build failed: {}",
                e
            )))
        })
}
