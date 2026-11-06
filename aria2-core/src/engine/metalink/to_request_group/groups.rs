use std::sync::{Arc, RwLock};

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::engine::metalink::request_graph::MetalinkRequestGraph;
use crate::error::{Aria2Error, Result};
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::request::request_group::BtFileMapping;
use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::metalink::parser::MetalinkDocument;
#[cfg(feature = "metalink")]
use aria2_protocol::metalink::parser::MetalinkFile;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use std::path::Path;

use super::{MetalinkGroupExpansion, MetalinkToRequestGroup};

impl MetalinkToRequestGroup {
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
    /// Parse and expand one Metalink document into direct-resource groups and
    /// torrent metadata graphs using one shared selection and normalization pass.
    #[cfg(feature = "metalink")]
    pub fn create_groups_from_bytes(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
        gids: &mut impl Iterator<Item = crate::request::request_group::GroupId>,
    ) -> Result<MetalinkGroupExpansion> {
        let doc = MetalinkDocument::parse(metalink_data, self.base_uri.as_deref())
            .map_err(Aria2Error::MetalinkParse)?;
        let mut resource_files = Vec::new();
        #[cfg(feature = "bittorrent")]
        let mut torrent_files = Vec::new();

        for (index, file) in self.prepare_files(&doc, options)? {
            #[cfg(feature = "bittorrent")]
            if file.has_torrent_metaurl() {
                torrent_files.push(file);
                continue;
            }
            resource_files.push((index, file));
        }

        let resource_groups =
            self.create_resource_groups(metalink_data, options, resource_files, gids)?;
        #[cfg(feature = "bittorrent")]
        let torrent_graphs = self.create_torrent_graphs(options, torrent_files, gids)?;

        Ok(MetalinkGroupExpansion {
            resource_groups,
            #[cfg(feature = "bittorrent")]
            torrent_graphs,
        })
    }

    fn create_resource_groups(
        &self,
        metalink_data: &[u8],
        options: &DownloadOptions,
        prepared: Vec<(usize, MetalinkFile)>,
        gids: &mut impl Iterator<Item = crate::request::request_group::GroupId>,
    ) -> Result<Vec<Arc<RwLock<crate::request::request_group::RequestGroup>>>> {
        let mut groups = Vec::new();
        for (index, file) in prepared {
            let urls: Vec<String> = file
                .get_sorted_urls()
                .into_iter()
                .filter(|url| url.is_non_p2p())
                .map(|url| url.url.clone())
                .collect();
            if urls.is_empty() {
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
            if self.pause_requested {
                group.recover().request_pause();
            }
            groups.push(group);
        }
        Ok(groups)
    }

    /// Build torrent metadata/payload graphs from the already selected files.
    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    fn create_torrent_graphs(
        &self,
        options: &DownloadOptions,
        source_files: Vec<MetalinkFile>,
        gids: &mut impl Iterator<Item = crate::request::request_group::GroupId>,
    ) -> Result<Vec<MetalinkRequestGraph>> {
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
                let graph = MetalinkRequestGraph::new_memory_with_fallback_and_mappings(
                    &metadata_uri,
                    &first.name,
                    options,
                    metadata_gid,
                    payload_gid,
                    fallback_uris,
                    file_mappings,
                )?;
                if self.pause_requested {
                    graph.metadata.recover().request_pause();
                    graph.payload.recover().request_pause();
                }
                Ok(graph)
            })
            .collect()
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
