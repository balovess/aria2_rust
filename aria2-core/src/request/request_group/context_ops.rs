//! DownloadContext interaction and URI lifecycle delegation methods.
//!
//! In C++ aria2, `RequestGroup` delegates URI management to `FileEntry`
//! objects via `DownloadContext`. This module implements that delegation:
//!
//! - When `DownloadContext` is set, initial URIs are transferred to the
//!   first `FileEntry`'s `remaining_uris`.
//! - URI lifecycle queries (remaining, spent, results) delegate to
//!   `FileEntry` when `DownloadContext` is available.
//! - When `DownloadContext` is not yet set, queries fall back to
//!   `RequestGroup.uris` (the initial URI list).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tracing::{debug, trace};

use crate::download::DownloadContext;
use crate::download::file_entry::UriResult;
use crate::util::rwlock_ext::RwLockRecover;

const MAX_CONNECTION_CONTEXTS: usize = 32;

/// Storage accounting for URI strings retained by a request group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UriMemoryStats {
    /// Number of URI string values currently retained by the group/context.
    pub stored_count: usize,
    /// Sum of string lengths, excluding allocator overhead.
    pub logical_bytes: usize,
    /// Sum of `String::capacity()` values.
    pub capacity_bytes: usize,
    /// Logical bytes belonging to repeated URI values.
    pub duplicate_logical_bytes: usize,
    /// Capacity bytes belonging to repeated URI values.
    pub duplicate_capacity_bytes: usize,
}

impl super::RequestGroup {
    /// Measure URI duplication across the group's fallback list and its
    /// download-context URI lifecycle lists. This is diagnostic only and
    /// does not change URI ownership or ordering.
    pub fn uri_memory_stats(&self) -> UriMemoryStats {
        let mut stats = UriMemoryStats::default();
        let mut seen: HashMap<String, usize> = HashMap::new();

        let mut account = |uri: &String| {
            stats.stored_count += 1;
            stats.logical_bytes += uri.len();
            stats.capacity_bytes += uri.capacity();
            if seen.insert(uri.clone(), uri.len()).is_some() {
                stats.duplicate_logical_bytes += uri.len();
                stats.duplicate_capacity_bytes += uri.capacity();
            }
        };

        for uri in &self.uris {
            account(uri);
        }

        if let Some(ctx) = self.download_context.recover().as_ref() {
            for entry in ctx.get_file_entries() {
                for uri in entry.remaining_uris() {
                    account(uri);
                }
                for uri in entry.spent_uris() {
                    account(uri);
                }
                for result in entry.uri_results() {
                    account(&result.uri);
                }
            }
        }

        stats
    }

    // ── DownloadContext Accessors ────────────────────────────────────────

    /// Get a shared reference to the `DownloadContext`, if set.
    ///
    /// Returns `None` if the download context has not been initialized yet
    /// (e.g. before torrent metadata is parsed for BT downloads).
    pub fn get_download_context(&self) -> Option<Arc<DownloadContext>> {
        self.download_context.recover().clone()
    }

    /// Mark the owned download context as whole-file checksum verified.
    pub fn set_checksum_verified(&self, verified: bool) {
        if let Some(ctx) = self.download_context.recover().as_ref() {
            ctx.set_checksum_verified(verified);
        }
    }

    /// Set the `DownloadContext` for this download and transfer initial URIs.
    ///
    /// When `DownloadContext` is set for the first time, the URIs from
    /// `RequestGroup.uris` are transferred to the first `FileEntry`'s
    /// `remaining_uris`. This mirrors the C++ flow where `RequestGroup`
    /// creates `DownloadContext` with URIs already in `FileEntry`.
    ///
    /// If the `DownloadContext` already has file entries with URIs (e.g.
    /// for BT downloads), the transfer is skipped to avoid duplication.
    ///
    /// # URI Transfer Logic
    ///
    /// In C++ aria2, URIs are added to `FileEntry` during the construction
    /// of `DownloadContext`. In Rust, `DownloadContext` may be created
    /// independently (e.g. by `BtDownloadCommand` after parsing torrent
    /// metadata), so we transfer URIs lazily at the point of attachment.
    pub fn set_download_context(&self, mut ctx: Arc<DownloadContext>) {
        if let Some(stats) = self.global_net_stat() {
            ctx.set_global_net_stat(stats);
        }
        let mut guard = self.download_context.recover_mut();

        // Transfer initial URIs to the first FileEntry's remaining_uris.
        // This mirrors C++ where RequestGroup constructor adds URIs to FileEntry.
        // We only do this if the first FileEntry has no remaining URIs yet
        // (i.e. this is a fresh context, not one loaded from torrent metadata).
        //
        // We use Arc::get_mut which requires exclusive ownership of the Arc.
        // Since we just received this Arc and it hasn't been stored yet, we
        // should have the only reference. If Arc::get_mut fails (shouldn't
        // happen here), we skip the URI transfer — the context is still
        // stored correctly.
        if let Some(ctx_ref) = Arc::get_mut(&mut ctx) {
            let first_fe = match ctx_ref.get_file_entries_mut().first_mut() {
                Some(fe) => fe,
                None => {
                    debug!(
                        gid = self.gid.value(),
                        "DownloadContext has no file entries, cannot transfer URIs"
                    );
                    *guard = Some(ctx);
                    return;
                }
            };

            // Only transfer if the FileEntry doesn't already have URIs
            // (BT downloads have URIs from torrent metadata, HTTP don't).
            if first_fe.remaining_uris().is_empty() && !self.uris.is_empty() {
                trace!(
                    gid = self.gid.value(),
                    uri_count = self.uris.len(),
                    "Transferring initial URIs to FileEntry"
                );
                for uri in &self.uris {
                    first_fe.add_uri(uri);
                }
            }
        }

        *guard = Some(ctx);
    }

    // ── URI Lifecycle Delegation ────────────────────────────────────────
    // These methods delegate to FileEntry via DownloadContext when available,
    // falling back to the initial URI list when context is not yet set.

    /// Return the remaining (not-yet-dispatched) URIs.
    ///
    /// Delegates to `FileEntry::remaining_uris()` when `DownloadContext`
    /// is set; otherwise returns a snapshot of the initial URI list.
    /// Mirrors C++ `RequestGroup::getRemainingUris()` which iterates
    /// file entries in the download context.
    pub fn get_remaining_uris(&self) -> Vec<String> {
        let guard = self.download_context.recover();
        if let Some(ref ctx) = *guard {
            let entries = ctx.get_file_entries();
            let mut uris = Vec::new();
            for fe in entries {
                if fe.is_requested() {
                    uris.extend(fe.remaining_uris().iter().cloned());
                }
            }
            uris
        } else {
            // Fallback: return initial URIs (none have been dispatched yet)
            self.uris.clone()
        }
    }

    /// Return the spent (already-dispatched) URIs.
    ///
    /// Delegates to `FileEntry::spent_uris()` when `DownloadContext`
    /// is set; otherwise returns empty (no URIs have been dispatched).
    /// Mirrors C++ `RequestGroup::getSpentUris()`.
    pub fn get_spent_uris(&self) -> Vec<String> {
        let guard = self.download_context.recover();
        if let Some(ref ctx) = *guard {
            let entries = ctx.get_file_entries();
            let mut uris = Vec::new();
            for fe in entries {
                if fe.is_requested() {
                    uris.extend(fe.spent_uris().iter().cloned());
                }
            }
            uris
        } else {
            Vec::new()
        }
    }

    /// Return all URIs (spent + remaining) across all requested file entries.
    ///
    /// Mirrors C++ `RequestGroup::getUris()` which collects URIs from
    /// all `FileEntry` objects.
    pub fn get_all_uris(&self) -> Vec<String> {
        let guard = self.download_context.recover();
        if let Some(ref ctx) = *guard {
            let entries = ctx.get_file_entries();
            let mut uris = Vec::new();
            for fe in entries {
                if fe.is_requested() {
                    uris.extend(fe.uris());
                }
            }
            uris
        } else {
            self.uris.clone()
        }
    }

    /// Return a snapshot of all URI entries and their current lifecycle state.
    pub fn uri_entries(&self) -> Vec<super::UriEntry> {
        let guard = self.download_context.recover();
        if let Some(ref ctx) = *guard {
            ctx.get_file_entries()
                .iter()
                .filter(|fe| fe.is_requested())
                .flat_map(|fe| {
                    fe.uris().into_iter().map(|uri| {
                        let status = if fe.remaining_uris().iter().any(|value| value == &uri) {
                            "waiting"
                        } else if fe.spent_uris().iter().any(|value| value == &uri) {
                            "used"
                        } else {
                            "spent"
                        };
                        super::UriEntry {
                            uri,
                            status: status.to_string(),
                        }
                    })
                })
                .collect()
        } else {
            self.uris
                .iter()
                .cloned()
                .map(|uri| super::UriEntry {
                    uri,
                    status: "waiting".to_string(),
                })
                .collect()
        }
    }

    /// Remove the requested URIs and add new URIs to the first requested file entry.
    ///
    /// When `position` is present, additions are inserted at that zero-based
    /// position in the same order as the input. This mirrors aria2's optional
    /// `changeUri` position argument; deletion happens before insertion.
    pub fn change_uris(
        &mut self,
        file_index: usize,
        del_uris: &[String],
        add_uris: &[String],
        position: Option<usize>,
    ) -> crate::error::Result<(usize, usize)> {
        let file_index = file_index.checked_sub(1).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument("file index must be at least 1".to_string())
        })?;
        let mut guard = self.download_context.recover_mut();
        if let Some(ctx) = guard.as_mut() {
            let ctx_inner = Arc::get_mut(ctx).ok_or_else(|| {
                crate::error::Aria2Error::InvalidArgument(
                    "download context is shared and cannot be changed".to_string(),
                )
            })?;
            let entry = ctx_inner
                .get_file_entries_mut()
                .get_mut(file_index)
                .filter(|fe| fe.is_requested())
                .ok_or_else(|| {
                    crate::error::Aria2Error::InvalidArgument(
                        "download context has no requested file entry".to_string(),
                    )
                })?;
            let mut deleted = 0;
            for uri in del_uris {
                deleted += entry.remove_uri(uri) as usize;
            }
            let added = match position {
                Some(mut position) => {
                    let mut added = 0;
                    for uri in add_uris {
                        if entry.insert_uri(uri, position) {
                            added += 1;
                            position = position.saturating_add(1);
                        }
                    }
                    added
                }
                None => entry.add_uris(add_uris),
            };
            return Ok((deleted, added));
        }

        let mut deleted = 0;
        if file_index != 0 {
            return Err(crate::error::Aria2Error::InvalidArgument(
                "file index is unavailable before download context initialization".to_string(),
            ));
        }
        self.uris.retain(|uri| {
            if del_uris.iter().any(|deleted_uri| deleted_uri == uri) {
                deleted += 1;
                false
            } else {
                true
            }
        });
        let added = match position {
            Some(mut position) => {
                let mut added = 0;
                for uri in add_uris {
                    if url::Url::parse(uri).is_err() {
                        continue;
                    }
                    let insert_position = position.min(self.uris.len());
                    self.uris.insert(insert_position, uri.clone());
                    position = position.saturating_add(1);
                    added += 1;
                }
                added
            }
            None => add_uris
                .iter()
                .filter(|uri| url::Url::parse(uri).is_ok())
                .map(|uri| {
                    self.uris.push(uri.clone());
                    1
                })
                .sum(),
        };
        Ok((deleted, added))
    }

    /// Return URI attempt results across all requested file entries.
    ///
    /// Delegates to `FileEntry::uri_results()` when `DownloadContext`
    /// is set. Mirrors C++ `RequestGroup::getUriResults()`.
    pub fn get_uri_results(&self) -> VecDeque<UriResult> {
        let guard = self.download_context.recover();
        if let Some(ref ctx) = *guard {
            let entries = ctx.get_file_entries();
            let mut results = VecDeque::new();
            for fe in entries {
                if fe.is_requested() {
                    results.extend(fe.uri_results().iter().cloned());
                }
            }
            results
        } else {
            VecDeque::new()
        }
    }

    /// Add a URI result record for the first requested file entry.
    ///
    /// Mirrors C++ `FileEntry::addURIResult()`. Called by download commands
    /// when a URI attempt completes (success or failure).
    /// Publish a real protocol connection for timeout/error handling.
    pub fn set_connection_context(&self, context: crate::network::ConnectionContext) {
        if let Ok(mut contexts) = self.connection_contexts.write() {
            if let Some(index) = contexts.iter().position(|existing| existing == &context) {
                contexts.remove(index);
            }
            if contexts.len() >= MAX_CONNECTION_CONTEXTS {
                contexts.remove(0);
            }
            contexts.push(context);
        }
    }

    /// Clear connection snapshots before a new command generation starts.
    pub fn clear_connection_contexts(&self) {
        if let Ok(mut contexts) = self.connection_contexts.write() {
            contexts.clear();
        }
    }

    /// Read all real protocol connections observed by this group.
    pub fn connection_contexts(&self) -> Vec<crate::network::ConnectionContext> {
        self.connection_contexts
            .read()
            .map(|contexts| contexts.clone())
            .unwrap_or_default()
    }

    /// Return the most recently observed peer for timeout attribution.
    ///
    /// The history remains available for diagnostics, but a timeout belongs
    /// to the latest active connection rather than every peer seen by a
    /// concurrent or mirror-aware command generation.
    pub fn latest_connection_context(&self) -> Option<crate::network::ConnectionContext> {
        self.connection_contexts
            .read()
            .ok()
            .and_then(|contexts| contexts.last().cloned())
    }

    pub fn add_uri_result(&self, uri: String, result_code: u16) {
        let mut guard = self.download_context.recover_mut();
        if let Some(ref mut ctx) = *guard
            && let Some(ctx_inner) = Arc::get_mut(ctx)
            && let Some(fe) = ctx_inner
                .get_file_entries_mut()
                .iter_mut()
                .find(|fe| fe.is_requested())
        {
            fe.add_uri_result(uri, result_code);
        }
        // If no DownloadContext yet, the result is lost. This is acceptable
        // because URI results are only meaningful after a download attempt.
    }

    /// Reuse spent URIs that have not produced errors.
    ///
    /// Delegates to `FileEntry::reuse_uri()` for the first requested file
    /// entry. Called when all remaining URIs have been exhausted.
    /// Mirrors C++ `FileEntry::reuseUri()`.
    pub fn reuse_uri(&self, ignore_hosts: &[String]) {
        let mut guard = self.download_context.recover_mut();
        if let Some(ref mut ctx) = *guard
            && let Some(ctx_inner) = Arc::get_mut(ctx)
            && let Some(fe) = ctx_inner
                .get_file_entries_mut()
                .iter_mut()
                .find(|fe| fe.is_requested())
        {
            fe.reuse_uri(ignore_hosts);
        }
    }

    /// Check whether any requested file entry has remaining URIs.
    ///
    /// Mirrors C++ `isUriSuppliedForRequestedFileEntry()`. Returns `true`
    /// if at least one requested file entry has URIs that haven't been tried.
    pub fn has_remaining_uris(&self) -> bool {
        let guard = self.download_context.recover();
        if let Some(ref ctx) = *guard {
            ctx.get_file_entries()
                .iter()
                .any(|fe| fe.is_requested() && !fe.remaining_uris().is_empty())
        } else {
            !self.uris.is_empty()
        }
    }

    /// Add a redirect URI to the first requested file entry's remaining URIs.
    ///
    /// Matches C++ aria2 behavior where `HttpSkipResponseCommand` adds
    /// redirect target URIs to the `FileEntry`'s URI pool so they can be
    /// used for future download attempts (e.g. when the original URI fails
    /// but the redirect target succeeded before).
    ///
    /// If no `DownloadContext` is set yet, the URI is appended to the
    /// group's initial URI list instead.
    pub fn add_redirect_uri(&mut self, uri: &str) {
        let mut guard = self.download_context.recover_mut();
        if let Some(ref mut ctx) = *guard {
            // Try to get exclusive mutable access to the inner DownloadContext.
            // Arc::get_mut succeeds only when we hold the sole Arc reference,
            // which is typical during download execution.
            if let Some(ctx_inner) = Arc::get_mut(ctx)
                && let Some(fe) = ctx_inner
                    .get_file_entries_mut()
                    .iter_mut()
                    .find(|fe| fe.is_requested())
            {
                if fe.add_uri(uri) {
                    trace!("Added redirect URI to FileEntry: {}", uri);
                }
                return;
            }
            // If Arc is shared (rare during download), fall back to the
            // initial URI list. The URI will be available for the next
            // download attempt via reuse_uri().
            drop(guard);
            self.uris.push(uri.to_string());
            trace!(
                "Added redirect URI to initial URI list (shared Arc): {}",
                uri
            );
        } else {
            drop(guard);
            self.uris.push(uri.to_string());
            trace!(
                "Added redirect URI to initial URI list (no context): {}",
                uri
            );
        }
    }
}
