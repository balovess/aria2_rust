//! File entry operations: accessors, lookup, filtering, and path management.

use tracing::{debug, trace};

use crate::download::download_context::DownloadContext;
use crate::download::file_entry::FileEntry;

impl DownloadContext {
    // -----------------------------------------------------------------------
    // Total Length / Knowledge
    // -----------------------------------------------------------------------

    /// Derive the total length from file entries.
    ///
    /// Returns `file_entries.last().last_offset()`, or 0 if empty.
    /// This matches the C++ implementation where total length is not stored
    /// independently but computed from the last file entry's offset + length.
    pub fn get_total_length(&self) -> u64 {
        self.file_entries
            .last()
            .map(|fe| fe.last_offset())
            .unwrap_or(0)
    }

    /// Whether the total download length is known.
    pub fn knows_total_length(&self) -> bool {
        self.knows_total_length
    }

    /// Mark the total length as unknown (e.g. content-length missing).
    pub fn mark_total_length_is_unknown(&mut self) {
        self.knows_total_length = false;
        debug!("Total length marked as unknown");
    }

    /// Mark the total length as known.
    pub fn mark_total_length_is_known(&mut self) {
        self.knows_total_length = true;
        debug!("Total length marked as known");
    }

    // -----------------------------------------------------------------------
    // File Entries
    // -----------------------------------------------------------------------

    /// Return a reference to the ordered file entry list.
    pub fn get_file_entries(&self) -> &[FileEntry] {
        &self.file_entries
    }

    /// Return a mutable reference to the ordered file entry list.
    pub fn get_file_entries_mut(&mut self) -> &mut Vec<FileEntry> {
        &mut self.file_entries
    }

    /// Return a reference to the first file entry.
    ///
    /// # Panics
    ///
    /// Panics if there are no file entries (matches C++ `assert`).
    pub fn get_first_file_entry(&self) -> &FileEntry {
        self.file_entries
            .first()
            .expect("get_first_file_entry: no file entries")
    }

    /// Return a reference to the first file entry whose `is_requested()` is true.
    ///
    /// Returns `None` if no such file entry exists.
    pub fn get_first_requested_file_entry(&self) -> Option<&FileEntry> {
        self.file_entries.iter().find(|fe| fe.is_requested())
    }

    /// Count the number of file entries whose `is_requested()` is true.
    pub fn count_requested_file_entry(&self) -> usize {
        self.file_entries
            .iter()
            .filter(|fe| fe.is_requested())
            .count()
    }

    /// Replace the file entry list with a new vector.
    pub fn set_file_entries(&mut self, entries: Vec<FileEntry>) {
        self.file_entries = entries;
        trace!(count = self.file_entries.len(), "File entries replaced");
    }

    /// Find the file entry that contains the given byte offset.
    ///
    /// Uses binary search over the sorted-by-offset file entries.
    /// Returns `None` if the offset is out of range or no file entries exist.
    ///
    /// # Algorithm
    ///
    /// Matches C++ `findFileEntryByOffset`:
    /// 1. Reject if empty or offset beyond the last file's end.
    /// 2. Use `partition_point` to find the insertion point for `offset`.
    /// 3. If the entry at the insertion point starts exactly at `offset`, return it.
    /// 4. Otherwise return the preceding entry (the file containing `offset`).
    pub fn find_file_entry_by_offset(&self, offset: u64) -> Option<&FileEntry> {
        if self.file_entries.is_empty() {
            return None;
        }
        let last_entry = self.file_entries.last().expect(
            "find_file_entry_by_offset: file_entries is non-empty but last() returned None",
        );
        let last_offset = last_entry.last_offset();
        if offset > 0 && last_offset <= offset {
            return None;
        }

        // partition_point: find first entry whose offset > the target offset
        let idx = self
            .file_entries
            .partition_point(|fe| fe.offset() <= offset);

        if idx > 0 {
            // The entry at idx-1 has offset <= our target.
            // If idx is in bounds and its offset == target, it's an exact match;
            // otherwise the preceding entry contains the offset.
            if idx < self.file_entries.len() && self.file_entries[idx].offset() == offset {
                Some(&self.file_entries[idx])
            } else {
                Some(&self.file_entries[idx - 1])
            }
        } else {
            // idx == 0 means offset < first entry's offset, which shouldn't
            // happen for valid offsets (offset 0 maps to the first entry).
            // But if the first entry starts at offset 0, partition_point returns 1.
            // If we reach here, the offset is before all entries.
            None
        }
    }

    // -----------------------------------------------------------------------
    // File Filter
    // -----------------------------------------------------------------------

    /// Mark file entries as requested / not-requested based on a list of
    /// 1-based indices.
    ///
    /// If the index list is empty or there is only one file entry, all
    /// entries are marked as requested. Otherwise, entries whose 1-based
    /// index appears in `indices` are marked requested; all others are
    /// marked not-requested.
    ///
    /// # Arguments
    ///
    /// * `indices` - Sorted, deduplicated, 1-based file indices.
    ///   Must be >= 1.
    pub fn set_file_filter(&mut self, mut indices: Vec<usize>) {
        indices.sort_unstable();
        indices.dedup();
        let ranges: Vec<_> = indices.into_iter().map(|index| index..=index).collect();
        self.set_file_filter_ranges(&ranges);
    }

    /// Mark file entries as requested / not-requested based on inclusive
    /// ranges of 1-based indices.
    ///
    /// This is the range-preserving form used by option parsing. It avoids
    /// expanding a large user range into a potentially unbounded temporary
    /// vector while keeping the same file-order semantics as
    /// [`Self::set_file_filter`].
    pub fn set_file_filter_ranges(&mut self, ranges: &[std::ops::RangeInclusive<usize>]) {
        // If no filter or single-file, all entries are requested
        if ranges.is_empty() || self.file_entries.len() <= 1 {
            for fe in &mut self.file_entries {
                fe.set_requested(true);
            }
            return;
        }

        for (i, fe) in self.file_entries.iter_mut().enumerate() {
            // Convert to 1-based index for comparison
            let one_based = i + 1;
            fe.set_requested(ranges.iter().any(|range| range.contains(&one_based)));
        }

        debug!(
            total = self.file_entries.len(),
            requested = self.count_requested_file_entry(),
            "File filter applied"
        );
    }

    /// Set the file path for the entry at the given 1-based index.
    ///
    /// # Errors
    ///
    /// Returns an error if `index` is 0 or exceeds the number of file entries.
    pub fn set_file_path_with_index(&mut self, index: usize, path: String) -> Result<(), String> {
        if index == 0 || index > self.file_entries.len() {
            return Err(format!("No such file with index={}", index));
        }
        // Path is not escaped here — matches C++ behavior
        self.file_entries[index - 1].set_path(path);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Path
    // -----------------------------------------------------------------------

    /// Return the representative path for this context.
    ///
    /// Used as part of the `.aria2` control file name. If `base_path` is set,
    /// returns `base_path`. Otherwise returns the first file entry's path.
    ///
    /// # Panics
    ///
    /// Panics if `base_path` is empty and there are no file entries.
    pub fn get_base_path(&self) -> &str {
        if !self.base_path.is_empty() {
            &self.base_path
        } else {
            self.get_first_file_entry().path()
        }
    }

    /// Set an override path for the `.aria2` control file naming.
    pub fn set_base_path(&mut self, path: String) {
        self.base_path = path;
    }

    // -----------------------------------------------------------------------
    // Post-download handler support
    // -----------------------------------------------------------------------

    /// Return the path of the first file entry, if any.
    ///
    /// Used by post-download handlers to determine the downloaded file's
    /// location. Mirrors C++ `RequestGroup::getFirstFilePath()`.
    pub fn first_file_path(&self) -> Option<&str> {
        self.file_entries
            .first()
            .map(|fe| fe.path())
            .filter(|s| !s.is_empty())
    }

    /// Return a reference to the first file entry, if any.
    ///
    /// Unlike `get_first_file_entry()` which panics if empty, this
    /// returns `None`. Used by post-download handlers to safely
    /// access file entry metadata.
    pub fn first_file_entry(&self) -> Option<&FileEntry> {
        self.file_entries.first()
    }

    // -----------------------------------------------------------------------
    // Resource Management
    // -----------------------------------------------------------------------

    /// Release runtime resources held by all file entries.
    ///
    /// Calls `put_back_request()` and `release_runtime_resource()` on each
    /// file entry, clearing in-memory download state while preserving the
    /// metadata needed for session persistence.
    pub fn release_runtime_resource(&mut self) {
        for fe in &mut self.file_entries {
            fe.put_back_request();
            fe.release_runtime_resource();
        }
        debug!(
            count = self.file_entries.len(),
            "Runtime resources released"
        );
    }
}
