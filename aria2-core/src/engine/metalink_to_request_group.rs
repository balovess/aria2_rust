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

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use std::path::Path;
use std::sync::{Arc, RwLock};

use tracing::{debug, info};

use crate::engine::metalink_download_command::MetalinkDownloadCommand;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::engine::metalink_request_graph::MetalinkRequestGraph;
use crate::error::{Aria2Error, Result};
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::request::request_group::BtFileMapping;
use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::metalink::parser::{
    MetalinkDocument, MetalinkFile, group_entry_by_metaurl_name,
};
use aria2_protocol::metalink::resource::LOWEST_PRIORITY;

/// Converts a Metalink document into download request groups.
///
/// Each file entry (or metaurl group) in the Metalink becomes a separate
/// download command that can be executed independently.
///
/// The conversion follows the C++ `Metalink2RequestGroup::createRequestGroup()`
/// algorithm:
/// 1. Drop unsupported resource types
/// 2. Skip entries with no resources and no metaurls
/// 3. Apply location-priority boosting
/// 4. Apply protocol-priority boosting
/// 5. Filter by select-file segment list
/// 6. Reorder metaurls by priority
/// 7. Group entries by metaurl name
/// 8. Create one request group per group
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

    /// Generate download commands from raw Metalink data in memory.
    ///
    /// Parses the Metalink document and creates download commands
    /// following the full C++ algorithm.
    ///
    /// Mirrors C++ `Metalink2RequestGroup::generate(groups, binaryStream, option, baseUri)`.
    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    pub fn create_torrent_graph(
        &self,
        file: &MetalinkFile,
        options: &DownloadOptions,
        metadata_gid: crate::request::request_group::GroupId,
        payload_gid: crate::request::request_group::GroupId,
    ) -> Result<MetalinkRequestGraph> {
        let metadata_uri = file
            .meta_urls
            .iter()
            .find(|metaurl| metaurl.mediatype.is_torrent() && !metaurl.url.is_empty())
            .map(|metaurl| metaurl.url.as_str())
            .ok_or_else(|| {
                Aria2Error::Fatal(crate::error::FatalError::Config(
                    "Metalink file has no torrent metaurl".to_string(),
                ))
            })?;

        let fallback_uris = file
            .get_sorted_urls()
            .into_iter()
            .filter(|url| url.is_non_p2p())
            .map(|url| url.url.clone())
            .collect();
        let file_mappings = Self::torrent_file_mappings(std::slice::from_ref(file), &[0], options);
        MetalinkRequestGraph::new_memory_with_fallback_and_mappings(
            metadata_uri,
            &file.name,
            options,
            metadata_gid,
            payload_gid,
            fallback_uris,
            file_mappings,
        )
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    fn torrent_file_mappings(
        files: &[MetalinkFile],
        indices: &[usize],
        options: &DownloadOptions,
    ) -> Vec<BtFileMapping> {
        let output_dir = options.dir.as_deref().unwrap_or(".");
        let max_connection_per_server = options
            .max_connection_per_server
            .unwrap_or(crate::constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
            .clamp(1, 16) as usize;

        indices
            .iter()
            .filter_map(|&index| files.get(index))
            .map(|file| BtFileMapping {
                original_name: file
                    .meta_urls
                    .iter()
                    .find(|metaurl| metaurl.mediatype.is_torrent())
                    .and_then(|metaurl| metaurl.name.clone())
                    .unwrap_or_default(),
                path: Path::new(output_dir)
                    .join(&file.name)
                    .to_string_lossy()
                    .into_owned(),
                uris: file
                    .get_sorted_urls()
                    .into_iter()
                    .filter(|url| url.is_non_p2p())
                    .map(|url| url.url.clone())
                    .collect(),
                max_connection_per_server,
                unique_protocol: options.metalink_enable_unique_protocol,
            })
            .collect()
    }

    /// Return whether a selected file mixes direct resources with torrent metaurls.
    #[cfg(feature = "metalink")]
    pub fn has_mixed_resource_torrent_entries(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
    ) -> Result<bool> {
        let doc = MetalinkDocument::parse(metalink_data, self.base_uri.as_deref())
            .map_err(Aria2Error::MetalinkParse)?;
        let version = if self.version.is_empty() {
            options.metalink_version.as_deref().unwrap_or("")
        } else {
            &self.version
        };
        let language = if self.language.is_empty() {
            options.metalink_language.as_deref().unwrap_or("")
        } else {
            &self.language
        };
        let os = if self.os.is_empty() {
            options.metalink_os.as_deref().unwrap_or("")
        } else {
            &self.os
        };
        Ok(doc
            .query_entries(version, language, os)
            .into_iter()
            .any(|index| {
                let file = &doc.files[index];
                let has_direct = !file
                    .get_sorted_urls()
                    .into_iter()
                    .filter(|url| url.is_non_p2p())
                    .collect::<Vec<_>>()
                    .is_empty();
                let has_torrent = file.has_torrent_metaurl();
                has_direct && has_torrent
            }))
    }

    /// Build manager-owned groups for Metalink files that contain direct resources.
    #[cfg(feature = "metalink")]
    pub fn create_resource_groups_from_bytes(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
        gids: &mut impl Iterator<Item = crate::request::request_group::GroupId>,
    ) -> Result<Vec<Arc<RwLock<crate::request::request_group::RequestGroup>>>> {
        let doc = MetalinkDocument::parse(metalink_data, self.base_uri.as_deref())
            .map_err(Aria2Error::MetalinkParse)?;
        let mut groups = Vec::new();
        for (index, file) in self.prepare_files(&doc, options)? {
            // With BitTorrent enabled, the first torrent metaurl owns the
            // whole Metalink group. It must not also become an independent
            // direct-resource group; the graph path below supplies its
            // metadata prerequisite and any direct fallback mirrors.
            let has_torrent_dependency = cfg!(feature = "bittorrent") && file.has_torrent_metaurl();
            if has_torrent_dependency {
                continue;
            }
            let urls: Vec<String> = file
                .get_sorted_urls()
                .into_iter()
                .filter(|url| url.is_non_p2p())
                .map(|url| url.url.clone())
                .collect();
            if urls.is_empty() {
                // Torrent-only entries are handled by the torrent graph path.
                continue;
            }
            let gid = gids.next().ok_or_else(|| {
                Aria2Error::Fatal(crate::error::FatalError::Config(
                    "Metalink resource GID allocator exhausted".to_string(),
                ))
            })?;
            let group = Arc::new(RwLock::new(
                crate::request::request_group::RequestGroup::new(gid, urls, options.clone()),
            ));
            group
                .recover()
                .set_metalink_source(metalink_data.to_vec(), index);
            group
                .recover()
                .set_metalink_base_uri(self.base_uri.as_deref());
            group.recover().set_output_name(file.name.clone());
            groups.push(group);
        }
        Ok(groups)
    }

    /// Build one metadata/payload graph for every filtered torrent-metaurl file.
    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    pub fn create_torrent_graphs_from_bytes(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
        gids: &mut impl Iterator<Item = crate::request::request_group::GroupId>,
    ) -> Result<Vec<MetalinkRequestGraph>> {
        let doc = MetalinkDocument::parse(metalink_data, self.base_uri.as_deref())
            .map_err(Aria2Error::MetalinkParse)?;
        let prepared = self.prepare_files(&doc, options)?;
        let mut source_files = Vec::new();
        for (_, file) in prepared {
            if file.has_torrent_metaurl() {
                source_files.push(file);
            }
        }
        if source_files.is_empty() {
            return Ok(Vec::new());
        }
        let groups = group_torrent_files_by_metaurl(&source_files);
        groups
            .into_iter()
            .filter(|(metaurl, _)| !metaurl.is_empty())
            .map(|(metadata_uri, indices)| {
                let first = &source_files[indices[0]];
                let fallback_uris = indices
                    .iter()
                    .flat_map(|&index| source_files[index].get_sorted_urls())
                    .filter(|url| url.is_non_p2p())
                    .map(|url| url.url.clone())
                    .collect();
                let metadata_gid = gids.next().ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "Metalink graph GID allocator exhausted".to_string(),
                    ))
                })?;
                let payload_gid = gids.next().ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "Metalink graph GID allocator exhausted".to_string(),
                    ))
                })?;
                let file_mappings = Self::torrent_file_mappings(&source_files, &indices, options);
                MetalinkRequestGraph::new_memory_with_fallback_and_mappings(
                    &metadata_uri,
                    &first.name,
                    options,
                    metadata_gid,
                    payload_gid,
                    fallback_uris,
                    file_mappings,
                )
            })
            .collect()
    }

    pub fn generate_from_bytes(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
    ) -> Result<Vec<MetalinkDownloadCommand>> {
        // DownloadOptions is the engine-level source of the C++ PREF_METALINK_*
        // filters. Explicit builder values remain useful for library callers;
        // options fill only fields that were not explicitly configured.
        let configured = Self {
            version: if self.version.is_empty() {
                options.metalink_version.clone().unwrap_or_default()
            } else {
                self.version.clone()
            },
            language: if self.language.is_empty() {
                options.metalink_language.clone().unwrap_or_default()
            } else {
                self.language.clone()
            },
            os: if self.os.is_empty() {
                options.metalink_os.clone().unwrap_or_default()
            } else {
                self.os.clone()
            },
            locations: if self.locations.is_empty() {
                options
                    .metalink_location
                    .as_deref()
                    .map(|value| {
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(|value| value.to_ascii_lowercase())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                self.locations.clone()
            },
            preferred_protocol: self
                .preferred_protocol
                .clone()
                .or_else(|| options.metalink_preferred_protocol.clone()),
            select_files: self.select_files.clone(),
            pause_requested: self.pause_requested,
            base_uri: self.base_uri.clone(),
        };
        configured.generate_from_bytes_with_config(metalink_data, options)
    }

    fn generate_from_bytes_with_config(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
    ) -> Result<Vec<MetalinkDownloadCommand>> {
        // Parse with base URI if available
        let doc = if let Some(ref base) = self.base_uri {
            // The parser accepts base_uri via MetalinkDocument::parse_with_base
            // For now, just parse and set base_uri afterwards
            let mut doc = MetalinkDocument::parse(metalink_data, self.base_uri.as_deref())
                .map_err(Aria2Error::MetalinkParse)?;
            doc.base_uri = Some(base.clone());
            doc
        } else {
            MetalinkDocument::parse(metalink_data, None).map_err(Aria2Error::MetalinkParse)?
        };

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
                commands.extend(file_infos.into_iter().map(|file_info| file_info.command));
            } else {
                commands.push(MetalinkDownloadCommand::create_multi_file_group(
                    &grouped_files,
                    options,
                    options.dir.as_deref(),
                    gid,
                )?);
            }
        }

        info!(
            count = commands.len(),
            "Metalink-to-request-group: generated download commands"
        );

        Ok(commands)
    }
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn group_torrent_files_by_metaurl(files: &[MetalinkFile]) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();

    for (index, file) in files.iter().enumerate() {
        let Some(metaurl) = file
            .meta_urls
            .iter()
            .find(|metaurl| metaurl.mediatype.is_torrent() && !metaurl.url.is_empty())
        else {
            continue;
        };

        // Match aria2's merge rule: unnamed or size-unknown members remain
        // independent even when they point at the same torrent URL.
        let can_merge =
            metaurl.name.as_deref().is_some_and(|name| !name.is_empty()) && file.size_known;
        let group_index = if can_merge {
            groups.iter().position(|(url, indices)| {
                let first = &files[indices[0]];
                let first_has_name = first
                    .meta_urls
                    .iter()
                    .find(|candidate| candidate.mediatype.is_torrent())
                    .and_then(|candidate| candidate.name.as_deref())
                    .is_some_and(|name| !name.is_empty());
                url == &metaurl.url && first_has_name
            })
        } else {
            None
        };

        if let Some(group_index) = group_index {
            groups[group_index].1.push(index);
        } else {
            groups.push((metaurl.url.clone(), vec![index]));
        }
    }

    groups
}

impl Default for MetalinkToRequestGroup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::DownloadOptions;
    use crate::util::rwlock_ext::RwLockRecover;

    #[test]
    fn options_location_is_applied_case_insensitively() {
        let options = DownloadOptions {
            metalink_location: Some(" US, jp ".to_string()),
            ..DownloadOptions::default()
        };
        let converter = MetalinkToRequestGroup::new();
        let configured = converter
            .generate_from_bytes(
                br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="x"><url location="us" priority="10">http://us/x</url><url location="de" priority="1">http://de/x</url></file></metalink>"#,
                &options,
            )
            .unwrap();
        assert_eq!(configured.len(), 1);

        let doc = MetalinkDocument::parse(
            br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="x"><url location="us" priority="10">http://us/x</url><url location="de" priority="1">http://de/x</url></file></metalink>"#,
            None,
        )
        .unwrap();
        let mut file = doc.files[0].clone();
        file.set_location_priority(&["us"], -LOWEST_PRIORITY);
        assert!(file.urls[0].priority < file.urls[1].priority);
    }

    fn make_multi_file_metalink() -> Vec<u8> {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="first.bin">
    <size>1024</size>
    <hash type="sha-256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</hash>
    <url priority="1">http://mirror.example.com/first.bin</url>
    <version>1.0</version>
    <language>en</language>
    <os>Linux</os>
  </file>
  <file name="second.bin">
    <size>2048</size>
    <hash type="sha-256">bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</hash>
    <url priority="1">http://mirror.example.com/second.bin</url>
    <version>2.0</version>
    <language>fr</language>
    <os>Windows</os>
  </file>
</metalink>"#
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn test_generate_from_bytes_no_filter() {
        let options = DownloadOptions::default();
        let converter = MetalinkToRequestGroup::new();
        let commands = converter
            .generate_from_bytes(&make_multi_file_metalink(), &options)
            .unwrap();
        assert_eq!(commands.len(), 2);
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn generate_from_bytes_groups_named_shared_torrent_metaurl_files() {
        let data = include_bytes!("../../tests/fixtures/grouped_metaurl.xml");
        let commands = MetalinkToRequestGroup::new()
            .generate_from_bytes(data, &DownloadOptions::default())
            .expect("grouped Metalink should convert");

        assert_eq!(commands.len(), 2, "file1/file3 share one payload group");
        let grouped = commands
            .iter()
            .find(|command| {
                command
                    .group()
                    .get_download_context()
                    .is_some_and(|context| context.get_file_entries().len() == 2)
            })
            .expect("shared torrent files should form one multi-file payload group");
        let context = grouped
            .group()
            .get_download_context()
            .expect("grouped payload should have a download context");
        let entries = context.get_file_entries();
        assert!(entries[0].path().ends_with("file1"));
        assert_eq!(entries[0].original_name(), "file1");
        assert_eq!(
            entries[0].remaining_uris().front().map(String::as_str),
            Some("http://file1p1")
        );
        assert!(entries[1].path().ends_with("file3"));
        assert_eq!(entries[1].original_name(), "file3");
        assert_eq!(
            entries[1].remaining_uris().front().map(String::as_str),
            Some("http://file3p1")
        );

        let independent = commands
            .iter()
            .find(|command| command.output_path().ends_with("file2"))
            .expect("file2 should remain independent");
        assert_eq!(
            independent.group().uris().iter().map(|uri| uri.as_ref()).collect::<Vec<_>>(),
            ["http://file2p1"]
        );
    }

    #[test]
    fn test_generate_from_bytes_version_filter() {
        let options = DownloadOptions::default();
        let converter = MetalinkToRequestGroup::new().with_version("1.0");
        let commands = converter
            .generate_from_bytes(&make_multi_file_metalink(), &options)
            .unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn test_generate_from_bytes_language_filter() {
        let options = DownloadOptions::default();
        let converter = MetalinkToRequestGroup::new().with_language("fr");
        let commands = converter
            .generate_from_bytes(&make_multi_file_metalink(), &options)
            .unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn test_generate_from_bytes_os_filter() {
        let options = DownloadOptions::default();
        let converter = MetalinkToRequestGroup::new().with_os("Linux");
        let commands = converter
            .generate_from_bytes(&make_multi_file_metalink(), &options)
            .unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn test_generate_from_bytes_no_match() {
        let options = DownloadOptions::default();
        let converter = MetalinkToRequestGroup::new().with_version("99.0");
        let commands = converter
            .generate_from_bytes(&make_multi_file_metalink(), &options)
            .unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn select_file_option_uses_query_result_positions() {
        let options = DownloadOptions {
            select_file: Some("2".to_string()),
            ..DownloadOptions::default()
        };
        let commands = MetalinkToRequestGroup::new()
            .generate_from_bytes(&make_multi_file_metalink(), &options)
            .expect("select-file should be accepted");
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0]
                .output_path()
                .file_name()
                .and_then(|name| name.to_str()),
            Some("second.bin")
        );
    }

    #[test]
    fn select_file_range_keeps_only_requested_entries() {
        let options = DownloadOptions {
            select_file: Some("1-2".to_string()),
            ..DownloadOptions::default()
        };
        let commands = MetalinkToRequestGroup::new()
            .generate_from_bytes(&make_multi_file_metalink(), &options)
            .expect("select-file range should be accepted");
        assert_eq!(commands.len(), 2);
    }

    #[cfg(feature = "metalink")]
    #[test]
    fn manager_resource_groups_apply_select_file_and_location_priority() {
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="first.bin"><url location="de" priority="1">https://de.example/first</url></file><file name="second.bin"><url location="us" priority="100">https://us.example/second</url><url location="de" priority="1">https://de.example/second</url></file></metalink>"#;
        let options = DownloadOptions {
            select_file: Some("2".to_string()),
            metalink_location: Some("us".to_string()),
            ..DownloadOptions::default()
        };
        let mut gids = [crate::request::request_group::GroupId::new(70)].into_iter();
        let groups = MetalinkToRequestGroup::new()
            .create_resource_groups_from_bytes(data, &options, &mut gids)
            .expect("filtered Metalink should create one resource group");

        assert_eq!(groups.len(), 1);
        let group = groups[0].recover();
        assert_eq!(group.output_name().as_deref(), Some("second.bin"));
        assert_eq!(
            group.uris().iter().map(|uri| uri.as_ref()).collect::<Vec<_>>(),
            ["https://us.example/second", "https://de.example/second"]
        );
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn create_torrent_graphs_allocates_metadata_and_payload_pairs() {
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="one.bin"><metaurl mediatype="torrent">https://example.test/one.torrent</metaurl></file><file name="two.bin"><metaurl mediatype="torrent">https://example.test/two.torrent</metaurl></file></metalink>"#;
        let converter = MetalinkToRequestGroup::new();
        let options = DownloadOptions::default();
        let mut gids = [
            crate::request::request_group::GroupId::new(40),
            crate::request::request_group::GroupId::new(41),
            crate::request::request_group::GroupId::new(42),
            crate::request::request_group::GroupId::new(43),
        ]
        .into_iter();
        let graphs = converter
            .create_torrent_graphs_from_bytes(data, &options, &mut gids)
            .expect("torrent-only Metalink should create graphs");
        assert_eq!(graphs.len(), 2);
        assert_eq!(
            graphs[0].metadata.recover().gid(),
            crate::request::request_group::GroupId::new(40)
        );
        assert_eq!(
            graphs[0].payload.recover().gid(),
            crate::request::request_group::GroupId::new(41)
        );
        assert_eq!(
            graphs[1].metadata.recover().gid(),
            crate::request::request_group::GroupId::new(42)
        );
        assert_eq!(
            graphs[1].payload.recover().gid(),
            crate::request::request_group::GroupId::new(43)
        );
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn shared_torrent_metaurl_creates_one_graph_with_all_fallbacks() {
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="first.bin"><size>10</size><url>https://example.test/first.bin</url><metaurl name="first.bin" mediatype="torrent">https://example.test/shared.torrent</metaurl></file><file name="second.bin"><size>20</size><url>https://example.test/second.bin</url><metaurl name="second.bin" mediatype="torrent">https://example.test/shared.torrent</metaurl></file></metalink>"#;
        let converter = MetalinkToRequestGroup::new();
        let options = DownloadOptions::default();
        let mut gids = [
            crate::request::request_group::GroupId::new(90),
            crate::request::request_group::GroupId::new(91),
        ]
        .into_iter();
        let graphs = converter
            .create_torrent_graphs_from_bytes(data, &options, &mut gids)
            .expect("shared torrent metaurl should create one graph");
        assert_eq!(graphs.len(), 1);
        assert_eq!(
            graphs[0].metadata.recover().uris().iter().map(|uri| uri.as_ref()).collect::<Vec<_>>(),
            ["https://example.test/shared.torrent"]
        );
        assert_eq!(
            graphs[0].payload.recover().gid(),
            crate::request::request_group::GroupId::new(91)
        );
        assert!(!graphs[0].payload.recover().is_dependency_resolved());
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn grouped_metaurl_fixture_has_metadata_payload_and_independent_groups() {
        let data = include_bytes!("../../tests/fixtures/grouped_metaurl.xml");
        let converter = MetalinkToRequestGroup::new();
        let options = DownloadOptions::default();
        let mut gids = (1..=6).map(crate::request::request_group::GroupId::new);

        let resource_groups = converter
            .create_resource_groups_from_bytes(data, &options, &mut gids)
            .expect("fixture resources should convert");
        let graphs = converter
            .create_torrent_graphs_from_bytes(data, &options, &mut gids)
            .expect("fixture torrent group should convert");

        assert_eq!(resource_groups.len(), 1);
        assert_eq!(
            resource_groups[0].recover().output_name().as_deref(),
            Some("file2")
        );
        assert_eq!(graphs.len(), 1);
        assert_eq!(
            graphs[0]
                .metadata
                .recover()
                .uris()
                .iter()
                .map(|uri| uri.as_ref())
                .collect::<Vec<_>>(),
            ["http://torrent"]
        );
        assert_eq!(
            graphs[0]
                .payload
                .recover()
                .uris()
                .iter()
                .map(|uri| uri.as_ref())
                .collect::<Vec<_>>(),
            ["bt://0000000000000002"]
        );

        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let torrent_data = {
            let mut file_entries = Vec::new();
            for name in ["file1", "file3"] {
                let path = BencodeValue::List(vec![BencodeValue::Bytes(name.as_bytes().to_vec())]);
                let mut file = BTreeMap::new();
                file.insert(b"length".to_vec(), BencodeValue::Int(1_000));
                file.insert(b"path".to_vec(), path);
                file_entries.push(BencodeValue::Dict(file));
            }
            let mut info = BTreeMap::new();
            info.insert(b"files".to_vec(), BencodeValue::List(file_entries));
            info.insert(b"name".to_vec(), BencodeValue::Bytes(b"bundle".to_vec()));
            info.insert(b"piece length".to_vec(), BencodeValue::Int(2_000));
            info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));
            let mut root = BTreeMap::new();
            root.insert(
                b"announce".to_vec(),
                BencodeValue::Bytes(b"https://tracker.test/announce".to_vec()),
            );
            root.insert(b"info".to_vec(), BencodeValue::Dict(info));
            BencodeValue::Dict(root).encode()
        };

        let graph = graphs.into_iter().next().expect("shared graph exists");
        let metadata = Arc::clone(&graph.metadata);
        let payload = Arc::clone(&graph.payload);
        metadata.recover().set_in_memory_data(torrent_data);
        let manager = crate::request::request_group_man::RequestGroupMan::new();
        manager
            .add_metalink_graph(graph)
            .expect("graph should be inserted atomically");
        assert_eq!(manager.fill_from_reserver().len(), 1);
        manager.resolve_dependencies_for_status(
            crate::request::request_group::GroupId::new(2),
            crate::request::request_group::DownloadStatus::Complete,
        );
        assert_eq!(manager.fill_from_reserver().len(), 1);
        let context = payload
            .recover()
            .get_download_context()
            .expect("payload context should be resolved");
        let entries = context.get_file_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].original_name(), "file1");
        assert_eq!(entries[1].original_name(), "file3");
        assert_eq!(
            entries[0].remaining_uris().front().map(String::as_str),
            Some("http://file1p1")
        );
        assert_eq!(
            entries[1].remaining_uris().front().map(String::as_str),
            Some("http://file3p1")
        );
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn shared_torrent_graph_maps_torrent_files_to_metalink_paths() {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="renamed/one.bin"><size>3</size><url>https://mirror.test/one.bin</url><metaurl name="dir1/file1.txt" mediatype="torrent">https://example.test/shared.torrent</metaurl></file><file name="renamed/two.bin"><size>2</size><url>https://mirror.test/two.bin</url><metaurl name="dir2/file2.dat" mediatype="torrent">https://example.test/shared.torrent</metaurl></file></metalink>"#;
        let converter = MetalinkToRequestGroup::new();
        let options = DownloadOptions::default();
        let mut gids = [
            crate::request::request_group::GroupId::new(100),
            crate::request::request_group::GroupId::new(101),
        ]
        .into_iter();
        let mut graphs = converter
            .create_torrent_graphs_from_bytes(data, &options, &mut gids)
            .expect("shared torrent graph should be created");
        assert_eq!(graphs.len(), 1);

        let torrent_data = {
            let mut file_entries = Vec::new();
            for (length, path) in [
                (3, vec!["dir1", "file1.txt"]),
                (2, vec!["dir2", "file2.dat"]),
            ] {
                let path = BencodeValue::List(
                    path.into_iter()
                        .map(|component| BencodeValue::Bytes(component.as_bytes().to_vec()))
                        .collect(),
                );
                let mut file = BTreeMap::new();
                file.insert(b"length".to_vec(), BencodeValue::Int(length));
                file.insert(b"path".to_vec(), path);
                file_entries.push(BencodeValue::Dict(file));
            }
            let mut info = BTreeMap::new();
            info.insert(b"files".to_vec(), BencodeValue::List(file_entries));
            info.insert(b"name".to_vec(), BencodeValue::Bytes(b"bundle".to_vec()));
            info.insert(b"piece length".to_vec(), BencodeValue::Int(5));
            info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));
            let mut root = BTreeMap::new();
            root.insert(
                b"announce".to_vec(),
                BencodeValue::Bytes(b"https://tracker.test/announce".to_vec()),
            );
            root.insert(b"info".to_vec(), BencodeValue::Dict(info));
            BencodeValue::Dict(root).encode()
        };

        let graph = graphs.pop().unwrap();
        let metadata = Arc::clone(&graph.metadata);
        let payload = Arc::clone(&graph.payload);
        metadata.recover().set_in_memory_data(torrent_data);
        let manager = crate::request::request_group_man::RequestGroupMan::new();
        manager.add_metalink_graph(graph).unwrap();
        assert_eq!(manager.fill_from_reserver().len(), 1);
        manager.resolve_dependencies_for_status(
            crate::request::request_group::GroupId::new(100),
            crate::request::request_group::DownloadStatus::Complete,
        );
        assert_eq!(manager.fill_from_reserver().len(), 1);

        let context = payload
            .recover()
            .get_download_context()
            .expect("resolved payload should have a context");
        let entries = context.get_file_entries();
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].path().ends_with("renamed\\one.bin")
                || entries[0].path().ends_with("renamed/one.bin")
        );
        assert!(
            entries[1].path().ends_with("renamed\\two.bin")
                || entries[1].path().ends_with("renamed/two.bin")
        );
        assert_eq!(
            entries[0].remaining_uris().front().map(String::as_str),
            Some("https://mirror.test/one.bin")
        );
        assert_eq!(
            entries[1].remaining_uris().front().map(String::as_str),
            Some("https://mirror.test/two.bin")
        );
        assert!(entries.iter().all(|entry| entry.is_requested()));
    }

    #[cfg(all(feature = "metalink", not(feature = "bittorrent")))]
    #[test]
    fn mixed_resource_group_is_retained_without_bittorrent_support() {
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#;
        let converter = MetalinkToRequestGroup::new();
        let mut gids = [crate::request::request_group::GroupId::new(90)].into_iter();
        let groups = converter
            .create_resource_groups_from_bytes(data, &DownloadOptions::default(), &mut gids)
            .expect("mixed Metalink should create one fallback group");
        assert_eq!(groups.len(), 1);
        let source = groups[0]
            .recover()
            .metalink_source()
            .expect("fallback source should be attached");
        assert_eq!(source.1, 0);
        assert_eq!(
            groups[0].recover().uris(),
            &["https://example.test/payload.bin"]
        );
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn mixed_resource_and_torrent_entry_uses_graph_fallback() {
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#;
        let converter = MetalinkToRequestGroup::new();
        let mut gids = [
            crate::request::request_group::GroupId::new(92),
            crate::request::request_group::GroupId::new(93),
        ]
        .into_iter();
        let graphs = converter
            .create_torrent_graphs_from_bytes(data, &DownloadOptions::default(), &mut gids)
            .expect("mixed Metalink should create a graph");
        assert_eq!(graphs.len(), 1);
        assert_eq!(
            graphs[0].metadata.recover().gid(),
            crate::request::request_group::GroupId::new(92)
        );
        assert_eq!(
            graphs[0].payload.recover().uris().iter().map(|uri| uri.as_ref()).collect::<Vec<_>>(),
            ["bt://000000000000005c"]
        );
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn torrent_graph_detects_torrent_metaurl_after_other_metaurl() {
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://mirror.example/payload.bin</url><metaurl mediatype="xml" priority="1">https://example.test/payload.meta</metaurl><metaurl mediatype="torrent" priority="2">https://example.test/payload.torrent</metaurl></file></metalink>"#;
        let mut gids = [
            crate::request::request_group::GroupId::new(94),
            crate::request::request_group::GroupId::new(95),
        ]
        .into_iter();
        let graphs = MetalinkToRequestGroup::new()
            .create_torrent_graphs_from_bytes(data, &DownloadOptions::default(), &mut gids)
            .expect("torrent metaurl should be detected regardless of position");

        assert_eq!(graphs.len(), 1);
        assert_eq!(
            graphs[0].metadata.recover().uris().iter().map(|uri| uri.as_ref()).collect::<Vec<_>>(),
            ["https://example.test/payload.torrent"]
        );
    }

    #[cfg(feature = "metalink")]
    #[test]
    fn mixed_resource_and_torrent_entry_is_detected() {
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#;
        let converter = MetalinkToRequestGroup::new();
        assert!(
            converter
                .has_mixed_resource_torrent_entries(data, &DownloadOptions::default())
                .expect("Metalink should parse")
        );
    }

    #[test]
    fn metaurl_only_torrent_entry_is_not_dropped() {
        let options = DownloadOptions::default();
        let converter = MetalinkToRequestGroup::new();
        let commands = converter
            .generate_from_bytes(
                br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload"><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#,
                &options,
            )
            .unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].group.read().unwrap().uris(),
            Vec::<Box<str>>::new()
        );

        let info = commands[0].file_info.as_ref().unwrap();
        assert_eq!(info.torrent_metaurls.len(), 1);
        assert_eq!(
            info.torrent_metaurls[0].url,
            "https://example.test/payload.torrent"
        );
    }

    #[test]
    fn test_default() {
        let _converter = MetalinkToRequestGroup::default();
    }
}
