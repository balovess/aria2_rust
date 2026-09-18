use super::model::{MetalinkDocument, MetalinkFile};

/// Group MetalinkFile entries by their first metaurl's URL.
///
/// Mirrors C++ `metalink::groupEntryByMetaurlName()` from `metalink_helper.cc`.
///
/// The grouping logic:
/// - Entries with **no metaurls** form their own group with an empty metaurl key.
/// - Entries whose first metaurl has an **empty name** or whose **size is unknown**
///   always start a new group (they cannot be merged into an existing group).
/// - Otherwise, the entry is merged into an existing group if its first metaurl URL
///   matches the group's key AND the group's first entry has a non-empty name.
/// - If no matching group is found, a new group is created.
///
/// Returns a vector of `(metaurl_key, Vec<index>)` where `index` refers to the
/// position within the input `files` slice.
pub fn group_entry_by_metaurl_name(files: &[MetalinkFile]) -> Vec<(String, Vec<usize>)> {
    let mut result: Vec<(String, Vec<usize>)> = Vec::new();

    for (idx, file) in files.iter().enumerate() {
        if file.meta_urls.is_empty() {
            // No metaurls → standalone group with empty key
            result.push((String::new(), vec![idx]));
        } else {
            let meta_url = &file.meta_urls[0];
            // C++ condition: if name is empty or size is unknown, skip merge search
            let can_merge =
                meta_url.name.as_ref().is_some_and(|n| !n.is_empty()) && file.size_known;

            let mut found = false;
            if can_merge {
                for group in &mut result {
                    let group_first_has_name = files[group.1[0]]
                        .meta_urls
                        .first()
                        .and_then(|m| m.name.as_deref())
                        .is_some_and(|n| !n.is_empty());
                    if group.0 == meta_url.url && group_first_has_name {
                        group.1.push(idx);
                        found = true;
                        break;
                    }
                }
            }

            if !found {
                result.push((meta_url.url.clone(), vec![idx]));
            }
        }
    }

    result
}

impl MetalinkDocument {
    pub fn single_file(&self) -> Option<&MetalinkFile> {
        if self.files.len() == 1 {
            Some(&self.files[0])
        } else {
            None
        }
    }

    pub fn all_urls(&self) -> Vec<&str> {
        self.files
            .iter()
            .flat_map(|f| f.urls.iter().map(|u| u.url.as_str()))
            .collect()
    }

    pub fn total_size(&self) -> Option<u64> {
        let mut total: u64 = 0;
        for f in &self.files {
            if let Some(size) = f.size {
                total += size;
            }
        }
        if total > 0 || self.files.is_empty() {
            Some(total)
        } else {
            None
        }
    }

    /// Query (filter) file entries matching the given version/language/os criteria.
    ///
    /// Mirrors C++ `Metalinker::queryEntry()`. Returns indices of matching files.
    /// Empty filter strings match everything.
    pub fn query_entries(&self, version: &str, language: &str, os: &str) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                if !version.is_empty() && f.version.as_deref() != Some(version) {
                    return false;
                }
                if !language.is_empty() && !f.contains_language(language) {
                    return false;
                }
                if !os.is_empty() && !f.contains_os(os) {
                    return false;
                }
                true
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Filter file entries by a select-file segment list (1-based indices).
    ///
    /// Mirrors C++ `Metalink2RequestGroup::createRequestGroup()` which
    /// applies `PREF_SELECT_FILE` to keep only the selected files.
    /// The `segments` parameter is a sorted list of 1-based file indices.
    /// Returns a new `MetalinkDocument` containing only the selected files.
    pub fn select_files(&self, segments: &[usize]) -> Self {
        if segments.is_empty() {
            return self.clone();
        }

        let selected: Vec<MetalinkFile> = segments
            .iter()
            .filter_map(|&seg| {
                // Segments are 1-based
                if seg > 0 && seg <= self.files.len() {
                    Some(self.files[seg - 1].clone())
                } else {
                    None
                }
            })
            .collect();

        let mut doc = Self {
            version: self.version,
            files: selected,
            generator: self.generator.clone(),
            origin: self.origin.clone(),
            published: self.published.clone(),
            base_uri: self.base_uri.clone(),
        };
        if doc.files.is_empty() {
            doc.files = self.files.clone();
        }
        doc
    }
}
