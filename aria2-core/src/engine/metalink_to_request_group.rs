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

use std::sync::{Arc, RwLock};

use tracing::{debug, info};

use crate::engine::metalink_download_command::MetalinkDownloadCommand;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::engine::metalink_request_graph::MetalinkRequestGraph;
use crate::error::{Aria2Error, Result};
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
        let priority_boost = -LOWEST_PRIORITY;

        for (_, file) in &mut files {
            file.drop_unsupported_resources();
            if !locations.is_empty() {
                let location_refs: Vec<&str> = locations.iter().map(String::as_str).collect();
                file.set_location_priority(&location_refs, priority_boost);
            }
            if let Some(protocol) = preferred_protocol {
                file.set_protocol_priority(protocol, priority_boost);
            }
            file.reorder_metaurls_by_priority();
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
            .find(|metaurl| {
                metaurl.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent
                    && !metaurl.url.is_empty()
            })
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
        MetalinkRequestGraph::new_with_fallback(
            metadata_uri,
            &file.name,
            options,
            metadata_gid,
            payload_gid,
            fallback_uris,
        )
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
                let has_torrent = file.meta_urls.iter().any(|metaurl| {
                    metaurl.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent
                });
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
            let has_torrent_dependency = cfg!(feature = "bittorrent")
                && file
                    .meta_urls
                    .first()
                    .is_some_and(|metaurl| metaurl.mediatype.is_torrent());
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
            if file
                .meta_urls
                .first()
                .is_some_and(|metaurl| metaurl.mediatype.is_torrent())
            {
                source_files.push(file);
            }
        }
        if source_files.is_empty() {
            return Ok(Vec::new());
        }
        let groups = group_entry_by_metaurl_name(&source_files);
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
                MetalinkRequestGraph::new_with_fallback(
                    &metadata_uri,
                    &first.name,
                    options,
                    metadata_gid,
                    payload_gid,
                    fallback_uris,
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
            // For each group, we create one MetalinkDownloadCommand.
            // In C++, multi-entry groups become multi-file RequestGroups;
            // single-entry groups become single-file RequestGroups.
            // For now we create one command per entry in the group.
            // TODO: When BT dependency injection is implemented, the metaurl_key
            // should create a separate torrent download that the group depends on.

            for &idx in &file_indices {
                let file = &files[idx];
                if file.urls.is_empty() && metaurl_key.is_empty() {
                    continue;
                }

                // Reorder resources by priority (shuffle + sort)
                // (mirrors C++ entry->reorderResourcesByPriority())
                // We do this on a clone to avoid mutating the shared data.
                let mut file_clone = file.clone();
                file_clone.reorder_resources_by_priority();

                let gid_start = (commands.len() as u64) + 1;
                let file_infos = MetalinkDownloadCommand::create_multi_file_for_single(
                    &file_clone,
                    options,
                    doc.base_uri.as_deref(),
                    gid_start,
                )?;

                for fi in file_infos {
                    commands.push(fi.command);
                }
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
            graphs[0].metadata.recover().uris(),
            &["https://example.test/shared.torrent"]
        );
        assert_eq!(
            graphs[0].payload.recover().gid(),
            crate::request::request_group::GroupId::new(91)
        );
        assert!(!graphs[0].payload.recover().is_dependency_resolved());
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
            graphs[0].payload.recover().uris(),
            &["bt://000000000000005c"]
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
            Vec::<String>::new()
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
