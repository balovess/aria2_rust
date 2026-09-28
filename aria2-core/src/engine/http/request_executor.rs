//! Admission and execution for HTTP Range requests.
//!
//! This module owns per-download request admission and assigns admitted ranges
//! across independent reqwest pools. Each pool is warmed once per authority so
//! HTTP/2 can multiplex later range streams over a bounded set of TCP sessions.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

#[cfg(test)]
use crate::engine::http::cookie_helper::CookieHelper;
use crate::engine::http::segment_downloader::{SegmentProgress, WriteChunk};
use crate::error::Result;
#[cfg(test)]
use crate::http::{AuthResolveOptions, HttpRequestPolicy};

const MAX_SERVER_CONCURRENCY: usize = 16;

mod admission;
mod executor;
#[cfg(test)]
#[path = "request_executor_h2_tests.rs"]
mod h2_tests;
#[cfg(test)]
mod tests;

use admission::AdmissionLease;
#[cfg(test)]
use admission::{ExecutorState, RunningTask};
pub use executor::HttpSegmentRequestExecutor;

/// One HTTP byte-range request admitted for execution.
pub struct HttpSegmentRequest {
    pub segment_index: u32,
    /// Authority bucket used for per-server request admission.
    pub authority_key: String,
    pub url: String,
    pub offset: u64,
    pub length: u64,
    pub cookie_header: Option<String>,
    pub(crate) progress: Arc<SegmentProgress>,
    pub write_tx: mpsc::Sender<WriteChunk>,
    pub expected_entity_length: u64,
}

/// Result returned after one Range request finishes.
pub struct HttpSegmentRequestResult {
    /// Internal identity used to reclaim the task handle when this completion
    /// event is consumed. The result channel is the completion signal, so the
    /// scheduler never needs to probe every handle for readiness.
    pub(crate) task_id: u64,
    pub segment_index: u32,
    pub authority_key: String,
    pub result: Result<u64>,
    pub peer_addr: Option<std::net::SocketAddr>,
    // The lease intentionally remains attached to the result. A completed
    // request still counts as in-flight until the scheduler consumes it.
    _lease: AdmissionLease,
}

/// Return the authority used for per-server request accounting.
pub fn authority_key(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    let port = url.port_or_known_default()?;
    Some(format!(
        "{}://{host}:{port}",
        url.scheme().to_ascii_lowercase()
    ))
}
