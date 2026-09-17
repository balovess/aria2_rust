use std::collections::HashMap;
use std::sync::Arc;

use crate::util::rwlock_ext::RwLockRecover;

impl super::super::RequestGroup {
    // ── Basic Accessors ─────────────────────────────────────────────────

    /// Resolve the effective minimum range split size from the task snapshot,
    /// falling back to the typed execution option when no snapshot exists.
    pub(crate) fn effective_min_split_size(&self) -> u64 {
        self.effective_option_snapshot()
            .and_then(|options| options.get("min-split-size").cloned())
            .and_then(|value| super::super::options::option_value_to_string(&value))
            .and_then(|value| crate::config::OptionValue::parse_size_str_checked(&value).ok())
            .filter(|value| *value > 0)
            .or_else(|| self.options.min_split_size.filter(|value| *value > 0))
            .unwrap_or(crate::constants::DEFAULT_MIN_SPLIT_SIZE)
    }

    /// Return the group ID.
    pub fn gid(&self) -> super::super::GroupId {
        self.gid
    }

    /// Return the initial URI list.
    ///
    /// Note: This returns the *initial* URIs provided when the group was
    /// created. For the current remaining/spent URI state, use
    /// `get_remaining_uris()` / `get_spent_uris()` which delegate to
    /// `FileEntry` via `DownloadContext`.
    pub fn uris(&self) -> &[Box<str>] {
        &self.uris
    }

    /// Replace the initial URI set before a download context is attached.
    ///
    /// Dependency fallbacks use this to remove the synthetic `bt://` dispatch
    /// URI after torrent metadata failed and continue with direct mirrors.
    pub fn replace_uris(&mut self, uris: Vec<String>) {
        self.uris = uris.into_iter().map(String::into_boxed_str).collect();
    }

    /// Set a per-group output filename, used by Metalink entries.
    pub fn set_output_name(&self, name: impl Into<String>) {
        *self.output_name.recover_mut() = Some(name.into());
    }

    /// Return the per-group output filename, if configured.
    pub fn output_name(&self) -> Option<String> {
        self.output_name.recover().clone()
    }

    /// Return a reference to the download options.
    pub fn options(&self) -> &super::super::DownloadOptions {
        &self.options
    }

    /// Cheap clone of the options `Arc` — O(1) refcount bump instead of
    /// deep-cloning all `Vec<String>` fields.
    pub fn options_arc(&self) -> Arc<super::super::DownloadOptions> {
        Arc::clone(&self.options)
    }

    /// Record the canonical option values that created this task.
    ///
    /// Callers set this while the group is constructed or restored. It is
    /// intentionally separate from runtime overrides so a later global option
    /// change cannot alter the observable task state. Typed fields are
    /// synchronized here; options without a typed field remain in the raw
    /// snapshot for protocol and session consumers.
    pub fn set_option_snapshot(&mut self, options: HashMap<String, serde_json::Value>) {
        let snapshot = crate::config::project_initial_options(options);
        let typed_options = Arc::make_mut(&mut self.options);
        for (key, value) in &snapshot {
            let _ = super::rpc_update::apply_rpc_option(typed_options, key, value);
        }
        self.option_snapshot = Some(snapshot);
    }

    /// Return the creation snapshot with only already-applied runtime changes
    /// overlaid. Pending changes remain absent until a restart applies them.
    pub fn effective_option_snapshot(&self) -> Option<HashMap<String, serde_json::Value>> {
        let mut options = self.option_snapshot.clone()?;
        options.extend(self.runtime_options());
        Some(crate::config::project_initial_options(options))
    }
}
