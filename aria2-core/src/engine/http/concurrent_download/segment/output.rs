use crate::filesystem::disk_writer::CachedDiskWriter;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::ConcurrentDownloader;

pub(super) struct PreparedOutput {
    pub(super) writer: CachedDiskWriter,
    pub(super) limiter: Option<RateLimiter>,
}

pub(super) fn prepare(
    dl: &ConcurrentDownloader,
    options: &DownloadOptions,
    total_length: u64,
) -> PreparedOutput {
    let use_mmap = dl.file_allocation == "mmap" && total_length >= dl.mmap_threshold;
    let writer = CachedDiskWriter::new_with_mmap_bytes(
        &dl.output_path,
        Some(total_length),
        options.disk_cache_size_bytes(),
        use_mmap,
    );
    let limiter = options
        .max_download_limit
        .filter(|&rate| rate > 0)
        .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)));
    if let Some(limiter) = &limiter {
        dl.group.recover().set_rate_limiter(limiter.clone());
    }
    PreparedOutput { writer, limiter }
}
