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

use std::collections::VecDeque;
use std::sync::Arc;

use tracing::{debug, trace};

use crate::download::DownloadContext;
use crate::download::file_entry::UriResult;
use crate::util::rwlock_ext::RwLockRecover;

impl super::RequestGroup {
    // ── DownloadContext Accessors ────────────────────────────────────────

    /// Get a shared reference to the `DownloadContext`, if set.
    ///
    /// Returns `None` if the download context has not been initialized yet
    /// (e.g. before torrent metadata is parsed for BT downloads).
    pub fn get_download_context(&self) -> Option<Arc<DownloadContext>> {
        self.download_context.recover().clone()
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
            if let Some(ctx_inner) = Arc::get_mut(ctx) {
                if let Some(fe) = ctx_inner
                    .get_file_entries_mut()
                    .iter_mut()
                    .find(|fe| fe.is_requested())
                {
                    if fe.add_uri(uri) {
                        trace!("Added redirect URI to FileEntry: {}", uri);
                    }
                    return;
                }
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
