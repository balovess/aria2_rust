use tracing::{info, warn};

use aria2_protocol::metalink::parser::{MetaUrlEntry, UrlEntry};

/// Parsed file information used by per-file command instances
/// created by `create_multi_file()`.
#[derive(Clone)]
pub(crate) struct FileDownloadInfo {
    /// Sorted URL entries for this file.
    pub(crate) sorted_urls: Vec<UrlEntry>,
    /// Expected file size (from Metalink).
    pub(crate) expected_size: Option<u64>,
    /// First hash entry for verification.
    pub(crate) hash_entry: Option<aria2_protocol::metalink::parser::HashEntry>,
    /// Per-chunk piece hashes (`<pieces>`) for chunk-level verification.
    pub(crate) pieces: Option<aria2_protocol::metalink::parser::PieceInfo>,
    /// Torrent metaurls (`mediatype="application/x-bittorrent"`), tried when
    /// no HTTP/FTP mirror succeeds. Mirrors C++ `BtDependency`.
    pub(crate) torrent_metaurls: Vec<MetaUrlEntry>,
}

// =========================================================================
// K3 — Metalink Priority Ordering Functions
// =========================================================================

/// Sort metalink URL resources by priority ascending, then by location preference.
///
/// Lower priority number means tried first (priority 1 before priority 10),
/// matching the C++ `MetalinkEntry::reorderResourcesByPriority()` which uses
/// `PriorityHigher` comparator: `res1->priority < res2->priority` (ascending).
/// Within same priority level, URLs matching the location preference are
/// preferred over non-matching ones.
///
/// # Arguments
///
/// * `resources` - Slice of UrlEntry resources to sort
/// * `location_preference` - Optional location code (e.g., "eu", "us", "jp")
///   to boost matching URLs within same priority level
///
/// # Returns
///
/// A vector of references sorted by:
/// 1. Priority ascending (lower priority number = tried first)
/// 2. Location preference match (matching locations first among equal priority)
pub fn select_mirrors_by_priority<'a>(
    resources: &'a [UrlEntry],
    location_preference: &str,
) -> Vec<&'a UrlEntry> {
    let mut sorted: Vec<&'a UrlEntry> = resources.iter().collect();

    sorted.sort_by(|a, b| {
        // Primary sort: priority ascending (lower priority number = more preferred)
        // Matches C++ PriorityHigher: res1->priority < res2->priority
        let pri_cmp = a.priority.cmp(&b.priority);
        if pri_cmp != std::cmp::Ordering::Equal {
            return pri_cmp;
        }

        // Secondary sort: location preference (if specified and non-empty)
        if !location_preference.is_empty() {
            let a_matches = a
                .location
                .as_ref()
                .map(|l| {
                    l.contains(location_preference) || location_preference.contains(l.as_str())
                })
                .unwrap_or(false);
            let b_matches = b
                .location
                .as_ref()
                .map(|l| {
                    l.contains(location_preference) || location_preference.contains(l.as_str())
                })
                .unwrap_or(false);

            // Prefer matching location when priorities are equal
            if a_matches != b_matches {
                return b_matches.cmp(&a_matches);
            }
        }

        std::cmp::Ordering::Equal
    });

    sorted
}

/// Try mirrors in priority order until one succeeds or all fail.
///
/// Iterates through sorted URL entries attempting download with each.
/// Returns immediately on first success, or error after all attempts fail.
pub async fn try_mirrors_with_failover<F, Fut>(
    sorted_urls: &[&UrlEntry],
    download_fn: F,
) -> std::result::Result<Vec<u8>, String>
where
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Vec<u8>, String>>,
{
    for (i, url_res) in sorted_urls.iter().enumerate() {
        info!(
            index = i,
            url = %url_res.url,
            priority = url_res.priority,
            "Trying mirror"
        );

        match download_fn(&url_res.url).await {
            Ok(data) => {
                info!(
                    index = i,
                    size = data.len(),
                    url = %url_res.url,
                    "Mirror succeeded"
                );
                return Ok(data);
            }
            Err(e) => {
                warn!(
                    index = i,
                    url = %url_res.url,
                    error = %e,
                    "Mirror failed, trying next"
                );
                continue;
            }
        }
    }

    Err(format!("All {} mirrors failed", sorted_urls.len()))
}
