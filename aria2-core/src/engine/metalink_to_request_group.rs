//! Metalink-to-Request-Group converter
//!
//! Converts a parsed Metalink document into download request groups,
//! one per file entry (or per metaurl group) in the Metalink.
//!
//! In C++ aria2, `Metalink2RequestGroup` is responsible for:
//! 1. Parsing a Metalink file (from disk or from a binary stream)
//! 2. Querying entries by version/language/os
//! 3. Applying URL priority ordering and location preferences
//! 4. Filtering unsupported resources
//! 5. Selecting specific files by index
//! 6. Grouping entries by metaurl name (for BT dependency injection)
//! 7. Creating `RequestGroup` objects for each group
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `MetalinkToRequestGroup` | `Metalink2RequestGroup` |
//! | `generate_from_file()` | `generate(groups, metalinkFile, option, baseUri)` |
//! | `generate_from_bytes()` | `generate(groups, binaryStream, option, baseUri)` |
//! | `create_request_groups()` | `createRequestGroup(groups, entries, option)` |

use tracing::{debug, info};

use crate::engine::metalink_download_command::MetalinkDownloadCommand;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadOptions;
use aria2_protocol::metalink::parser::{
    MetalinkDocument, MetalinkFile, group_entry_by_metaurl_name,
};
use aria2_protocol::metalink::resource::LOWEST_PRIORITY;

mod groups;
#[cfg(test)]
mod tests;
pub struct MetalinkToRequestGroup {
    /// Optional base URI for resolving relative URLs in the Metalink.
    base_uri: Option<String>,
    /// Version filter (mirrors C++ `PREF_METALINK_VERSION`).
    version: String,
    /// Language filter (mirrors C++ `PREF_METALINK_LANGUAGE`).
    language: String,
    /// OS filter (mirrors C++ `PREF_METALINK_OS`).
    os: String,
    /// Location preference codes (mirrors C++ `PREF_METALINK_LOCATION`).
    locations: Vec<String>,
    /// Preferred protocol (mirrors C++ `PREF_METALINK_PREFERRED_PROTOCOL`).
    preferred_protocol: Option<String>,
    /// Select-file segments (1-based indices, mirrors C++ `PREF_SELECT_FILE`).
    select_files: Vec<usize>,
    /// Whether to pause newly created groups.
    pause_requested: bool,
}

impl MetalinkToRequestGroup {
    /// Create a new converter with default options.
    pub fn new() -> Self {
        Self {
            base_uri: None,
            version: String::new(),
            language: String::new(),
            os: String::new(),
            locations: Vec::new(),
            preferred_protocol: None,
            select_files: Vec::new(),
            pause_requested: false,
        }
    }

    /// Set the base URI for resolving relative URLs.
    pub fn with_base_uri(mut self, base_uri: impl Into<String>) -> Self {
        self.base_uri = Some(base_uri.into());
        self
    }

    /// Set version filter.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Set language filter.
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    /// Set OS filter.
    pub fn with_os(mut self, os: impl Into<String>) -> Self {
        self.os = os.into();
        self
    }

    /// Set location preference codes (comma-separated or pre-split).
    pub fn with_locations(mut self, locations: Vec<String>) -> Self {
        self.locations = locations;
        self
    }

    /// Set preferred protocol (e.g. "http", "https", "ftp").
    pub fn with_preferred_protocol(mut self, protocol: impl Into<String>) -> Self {
        let proto = protocol.into();
        if proto != "none" && !proto.is_empty() {
            self.preferred_protocol = Some(proto);
        }
        self
    }

    /// Set select-file segments (1-based file indices to keep).
    pub fn with_select_files(mut self, segments: Vec<usize>) -> Self {
        self.select_files = segments;
        self
    }

    /// Set whether newly created groups should be paused.
    pub fn with_pause_requested(mut self, pause: bool) -> Self {
        self.pause_requested = pause;
        self
    }

    /// Parse the C++ `PREF_SELECT_FILE` syntax used by Metalink.
    ///
    /// The returned values are 1-based positions in the filtered Metalink
    /// entry list, matching `Metalink2RequestGroup::createRequestGroup()`.
    fn parse_select_files(value: &str) -> Result<Vec<usize>> {
        let mut result = Vec::new();
        for segment in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((start, end)) = segment.split_once('-') {
                let start = start.trim().parse::<usize>().map_err(|_| {
                    Aria2Error::MetalinkParse(format!("invalid select-file segment `{segment}`"))
                })?;
                let end = end.trim().parse::<usize>().map_err(|_| {
                    Aria2Error::MetalinkParse(format!("invalid select-file segment `{segment}`"))
                })?;
                if start == 0 || end < start {
                    return Err(Aria2Error::MetalinkParse(format!(
                        "invalid select-file segment `{segment}`"
                    )));
                }
                result.extend(start..=end);
            } else {
                let index = segment.parse::<usize>().map_err(|_| {
                    Aria2Error::MetalinkParse(format!("invalid select-file segment `{segment}`"))
                })?;
                if index == 0 {
                    return Err(Aria2Error::MetalinkParse(format!(
                        "invalid select-file segment `{segment}`"
                    )));
                }
                result.push(index);
            }
        }
        result.sort_unstable();
        result.dedup();
        Ok(result)
    }

    /// Return the effective select-file positions, giving explicit builder
    /// configuration precedence over the per-download option.
    fn effective_select_files(&self, options: &DownloadOptions) -> Result<Vec<usize>> {
        if !self.select_files.is_empty() {
            return Ok(self.select_files.clone());
        }
        options
            .select_file
            .as_deref()
            .map(Self::parse_select_files)
            .transpose()
            .map(|segments| segments.unwrap_or_default())
    }

    /// Apply the Metalink resource rules that are needed by every execution
    /// path, including manager-owned groups that keep the raw document for
    /// restart/fallback handling.
    pub(crate) fn normalize_file_for_runtime(
        &self,
        file: &mut MetalinkFile,
        options: &DownloadOptions,
    ) {
        let locations: Vec<String> = if self.locations.is_empty() {
            options
                .metalink_location
                .as_deref()
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|location| !location.is_empty())
                        .map(str::to_ascii_lowercase)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            self.locations.clone()
        };
        let preferred_protocol = self
            .preferred_protocol
            .as_deref()
            .or(options.metalink_preferred_protocol.as_deref())
            .filter(|protocol| !protocol.eq_ignore_ascii_case("none"));

        file.drop_unsupported_resources();
        if !locations.is_empty() {
            let location_refs: Vec<&str> = locations.iter().map(String::as_str).collect();
            file.set_location_priority(&location_refs, -LOWEST_PRIORITY);
        }
        if let Some(protocol) = preferred_protocol {
            file.set_protocol_priority(protocol, -LOWEST_PRIORITY);
        }
        file.reorder_metaurls_by_priority();
    }

    /// Parse, query, select, and normalize Metalink entries once for all
    /// manager-owned construction paths.
    ///
    /// Keeping this operation behind one internal seam prevents the resource
    /// and torrent graph paths from silently disagreeing about filters,
    /// priorities, or source indices.
    fn prepare_files(
        &self,
        doc: &MetalinkDocument,
        options: &DownloadOptions,
    ) -> Result<Vec<(usize, MetalinkFile)>> {
        let version = if self.version.is_empty() {
            options.metalink_version.as_deref().unwrap_or("")
        } else {
            self.version.as_str()
        };
        let language = if self.language.is_empty() {
            options.metalink_language.as_deref().unwrap_or("")
        } else {
            self.language.as_str()
        };
        let os = if self.os.is_empty() {
            options.metalink_os.as_deref().unwrap_or("")
        } else {
            self.os.as_str()
        };

        let queried: Vec<(usize, MetalinkFile)> = doc
            .query_entries(version, language, os)
            .into_iter()
            .filter_map(|index| doc.files.get(index).cloned().map(|file| (index, file)))
            .collect();
        let select_files = self.effective_select_files(options)?;
        let mut files: Vec<(usize, MetalinkFile)> = if select_files.is_empty() {
            queried
        } else {
            // C++ applies select-file after queryEntry(), so indices refer to
            // the filtered list rather than the original XML positions.
            select_files
                .into_iter()
                .filter_map(|position| queried.get(position - 1).cloned())
                .collect()
        };

        for (_, file) in &mut files {
            self.normalize_file_for_runtime(file, options);
        }

        files.retain(|(_, file)| !file.urls.is_empty() || !file.meta_urls.is_empty());
        Ok(files)
    }

    /// Generate download commands from a Metalink file on disk.
    ///
    /// Reads the file, parses it, and creates one `MetalinkDownloadCommand`
    /// per file entry (or per metaurl group).
    ///
    /// Mirrors C++ `Metalink2RequestGroup::generate(groups, metalinkFile, option, baseUri)`.
    pub fn generate_from_file(
        &self,
        path: &std::path::Path,
        options: &DownloadOptions,
    ) -> Result<Vec<MetalinkDownloadCommand>> {
        let data = std::fs::read(path).map_err(|e| Aria2Error::Io(e.to_string()))?;
        self.generate_from_bytes(&data, options)
    }

    pub fn generate_from_bytes(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
    ) -> Result<Vec<MetalinkDownloadCommand>> {
        let doc = MetalinkDocument::parse(metalink_data, self.base_uri.as_deref())
            .map_err(Aria2Error::MetalinkParse)?;
        self.create_request_groups(doc, options)
    }

    /// Core conversion logic: MetalinkDocument → download commands.
    ///
    /// Mirrors C++ `Metalink2RequestGroup::createRequestGroup()`.
    fn create_request_groups(
        &self,
        doc: MetalinkDocument,
        options: &DownloadOptions,
    ) -> Result<Vec<MetalinkDownloadCommand>> {
        let files: Vec<MetalinkFile> = self
            .prepare_files(&doc, options)?
            .into_iter()
            .map(|(_, file)| file)
            .collect();

        if files.is_empty() {
            info!("No Metalink entries with supported resources remain after filtering");
            return Ok(Vec::new());
        }

        // Step 7: Group entries by metaurl name
        // (mirrors C++ metalink::groupEntryByMetaurlName)
        let groups = group_entry_by_metaurl_name(&files);

        debug!(
            "Metalink: {} files grouped into {} request groups",
            files.len(),
            groups.len()
        );

        // Step 8: Create download commands for each group
        let mut commands = Vec::with_capacity(groups.len());

        for (metaurl_key, file_indices) in groups {
            let grouped_files: Vec<MetalinkFile> = file_indices
                .iter()
                .filter_map(|&idx| files.get(idx))
                .filter(|file| !file.urls.is_empty() || !metaurl_key.is_empty())
                .cloned()
                .map(|mut file| {
                    // Reorder resources by priority once per selected entry.
                    // The grouped command retains the resulting per-file URI
                    // lists in its DownloadContext.
                    file.reorder_resources_by_priority();
                    file
                })
                .collect();

            if grouped_files.is_empty() {
                continue;
            }

            let gid = (commands.len() as u64) + 1;
            if grouped_files.len() == 1 {
                let file_infos = MetalinkDownloadCommand::create_multi_file_for_single(
                    &grouped_files[0],
                    options,
                    options.dir.as_deref(),
                    gid,
                )?;
                if self.pause_requested {
                    for file_info in &file_infos {
                        file_info.command.group().request_pause();
                    }
                }
                commands.extend(file_infos.into_iter().map(|file_info| file_info.command));
            } else {
                let command = MetalinkDownloadCommand::create_multi_file_group(
                    &grouped_files,
                    options,
                    options.dir.as_deref(),
                    gid,
                )?;
                if self.pause_requested {
                    command.group().request_pause();
                }
                commands.push(command);
            }
        }

        info!(
            count = commands.len(),
            "Metalink-to-request-group: generated download commands"
        );

        Ok(commands)
    }
}

impl Default for MetalinkToRequestGroup {
    fn default() -> Self {
        Self::new()
    }
}
