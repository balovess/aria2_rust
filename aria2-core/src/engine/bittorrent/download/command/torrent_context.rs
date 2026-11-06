use crate::config::{parse_index_out, parse_integer_segments};
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::{BtFileMapping, DownloadOptions, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;
use crate::util::uri::percent_encode;

fn normalized_announce_list(announce_list: &[Vec<String>], announce: &str) -> Vec<Vec<String>> {
    if announce_list.is_empty() && !announce.is_empty() {
        vec![vec![announce.to_string()]]
    } else {
        announce_list.to_vec()
    }
}

fn normalized_web_seed_list(torrent_seeds: &[String], additional_seeds: &[String]) -> Vec<String> {
    let mut seeds = Vec::with_capacity(torrent_seeds.len() + additional_seeds.len());
    seeds.extend(torrent_seeds.iter().cloned());
    seeds.extend(additional_seeds.iter().cloned());
    seeds.sort_unstable();
    seeds.dedup();
    seeds
}

fn file_web_seed_urls(
    web_seeds: &[String],
    torrent_name: &str,
    file_path: &[String],
    single_file: bool,
) -> Vec<String> {
    if single_file {
        let filename = percent_encode(torrent_name);
        return web_seeds
            .iter()
            .map(|seed| {
                if seed.ends_with('/') {
                    format!("{seed}{filename}")
                } else {
                    seed.clone()
                }
            })
            .collect();
    }

    let mut path = String::with_capacity(
        torrent_name.len() + file_path.iter().map(String::len).sum::<usize>() + file_path.len(),
    );
    path.push_str(&percent_encode(torrent_name));
    for component in file_path {
        path.push('/');
        path.push_str(&percent_encode(component));
    }
    web_seeds
        .iter()
        .map(|seed| {
            if seed.ends_with('/') {
                format!("{seed}{path}")
            } else {
                format!("{seed}/{path}")
            }
        })
        .collect()
}

/// Build the protocol-specific context that aria2 installs after torrent
/// metadata has been resolved.
///
/// Keeping this separate from command construction lets a dependency resolve
/// torrent metadata into an existing payload RequestGroup.
pub(crate) fn build_download_context_from_meta(
    meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
    path: String,
    additional_web_seeds: &[String],
) -> crate::error::Result<crate::download::DownloadContext> {
    use crate::download::DownloadContext;
    use crate::download::download_context::{BtFileMode, ContextAttributeType, TorrentAttribute};
    use crate::download::file_entry::FileEntry;

    let web_seeds = normalized_web_seed_list(&meta.web_seeds, additional_web_seeds);
    let is_single_file = meta.is_single_file();
    let mut ctx = if is_single_file {
        DownloadContext::new(meta.info.piece_length, meta.total_size(), path)
    } else {
        let base_dir = std::path::Path::new(&path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(&meta.info.name);
        let v1_files = meta.info.files.as_deref();
        let v2_files = meta.info.v2_files.as_deref();
        let file_count = v1_files
            .map(|files| {
                files
                    .iter()
                    .filter(|file| {
                        !file
                            .path
                            .first()
                            .is_some_and(|component| component == ".pad")
                    })
                    .count()
            })
            .or_else(|| v2_files.map(|files| files.len()))
            .unwrap_or(0);
        let mut entries = Vec::with_capacity(file_count);
        let mut offset = 0u64;
        let mut add_entry = |length: u64, path: &Vec<String>, offset: &mut u64| {
            let original_name = path.join("/");
            let file_path = base_dir.join(std::path::Path::new(&original_name));
            let web_seed_urls = file_web_seed_urls(&web_seeds, &meta.info.name, path, false);
            let mut entry = FileEntry::new(
                file_path.to_string_lossy().into_owned(),
                length,
                *offset,
                web_seed_urls,
            );
            entry.set_original_name(original_name.clone());
            entry.set_suffix_path(original_name);
            entries.push(entry);
            *offset = offset.saturating_add(length);
        };
        if let Some(files) = v1_files {
            for file in files {
                if file
                    .path
                    .first()
                    .is_some_and(|component| component == ".pad")
                {
                    offset = offset.saturating_add(file.length);
                    continue;
                }
                add_entry(file.length, &file.path, &mut offset);
            }
        } else if let Some(files) = v2_files {
            for file in files {
                offset =
                    offset.div_ceil(meta.info.piece_length as u64) * meta.info.piece_length as u64;
                add_entry(file.length, &file.path, &mut offset);
            }
        }
        let mut context = DownloadContext::new_default();
        context.set_piece_length(meta.info.piece_length);
        context.set_file_entries(entries);
        context
    };
    if meta.is_single_file()
        && let Some(entry) = ctx.get_file_entries_mut().first_mut()
    {
        entry.set_original_name(meta.info.name.clone());
        entry.set_suffix_path(meta.info.name.clone());
        let web_seed_urls = file_web_seed_urls(&web_seeds, &meta.info.name, &[], true);
        entry.add_uris(&web_seed_urls);
    }
    if meta.info.meta_version != Some(2) {
        let piece_hashes_hex: Vec<String> = meta.info.pieces.iter().map(hex::encode).collect();
        ctx.set_piece_hashes("sha-1".to_string(), piece_hashes_hex);
    }

    let torrent_attr = TorrentAttribute {
        name: meta.info.name.clone(),
        mode: if meta.is_single_file() {
            BtFileMode::Single
        } else {
            BtFileMode::Multi
        },
        announce_list: normalized_announce_list(&meta.announce_list, &meta.announce),
        nodes: meta.nodes.clone(),
        info_hash: meta.info_hash.as_hex(),
        metadata: Vec::new(),
        metadata_size: 0,
        private_torrent: meta.is_private(),
        creation_date: meta.creation_date.unwrap_or(0),
        comment: meta.comment.clone().unwrap_or_default(),
        created_by: meta.created_by.clone().unwrap_or_default(),
        url_list: web_seeds,
    };
    ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(torrent_attr));
    Ok(ctx)
}

/// Parse local torrent metadata into an existing request group before the
/// download command is promoted.
///
/// The original libaria2 path has a parsed `DownloadContext` available to a
/// caller after `addTorrent`, even when the task is paused. Keeping this
/// preparation separate from command construction gives RPC and library
/// callers the same observable file metadata without starting network work.
pub fn prepare_group_metadata(
    group: std::sync::Arc<std::sync::RwLock<RequestGroup>>,
    torrent_bytes: &[u8],
    options: &DownloadOptions,
    output_dir: Option<&str>,
    additional_web_seeds: &[String],
) -> Result<()> {
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
        .map_err(|error| Aria2Error::BittorrentParse(format!("Torrent parse failed: {error}")))?;
    let dir = output_dir
        .map(str::to_owned)
        .or_else(|| options.dir.clone())
        .unwrap_or_else(|| ".".to_string());
    let path = std::path::PathBuf::from(&dir).join(&meta.info.name);
    let mut context = build_download_context_from_meta(
        &meta,
        path.to_string_lossy().into_owned(),
        additional_web_seeds,
    )?;
    apply_index_out_paths(&mut context, options.index_out.as_deref(), &dir)?;
    apply_select_file_filter(&mut context, options.select_file.as_deref())?;

    let group = group.recover();
    group.set_bt_metadata(
        meta.num_pieces() as u32,
        meta.info.piece_length,
        meta.info_hash.as_hex(),
    );
    group.set_download_context(std::sync::Arc::new(context));
    Ok(())
}

/// Apply Metalink-selected paths and mirrors to a parsed torrent context.
///
/// The torrent parser owns the canonical file order and byte offsets. This
/// helper only changes the selected entries' destination and URI metadata, so
/// both dependency resolution and command fallback use the same mapping rule.
pub(crate) fn apply_file_mappings(
    context: &mut crate::download::DownloadContext,
    mappings: &[BtFileMapping],
) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }

    let entries = context.get_file_entries_mut();
    if entries.len() == 1 && mappings.len() == 1 && mappings[0].original_name.is_empty() {
        apply_file_mapping(&mut entries[0], &mappings[0]);
        return Ok(());
    }

    for entry in entries.iter_mut() {
        entry.set_requested(false);
    }

    for mapping in mappings {
        let entry = entries
            .iter_mut()
            .find(|entry| entry.original_name() == mapping.original_name)
            .ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "No entry '{}' in torrent metadata",
                    mapping.original_name
                )))
            })?;
        apply_file_mapping(entry, mapping);
    }
    Ok(())
}

fn apply_file_mapping(entry: &mut crate::download::file_entry::FileEntry, mapping: &BtFileMapping) {
    entry.set_requested(true);
    entry.set_path(mapping.path.clone());
    entry.add_uris(&mapping.uris);
    entry.set_max_connection_per_server(mapping.max_connection_per_server);
    entry.set_unique_protocol(mapping.unique_protocol);
}

/// Apply the user-facing `select-file` syntax to a torrent context.
///
/// The parser is shared with the rest of the configuration system. File
/// indices remain 1-based at this seam, while the context owns the mapping to
/// requested file entries. An empty value has the same meaning as an omitted
/// filter: request every file.
pub(crate) fn apply_select_file_filter(
    context: &mut crate::download::DownloadContext,
    select_file: Option<&str>,
) -> Result<()> {
    let Some(select_file) = select_file else {
        return Ok(());
    };

    if select_file.trim().is_empty() {
        context.set_file_filter(Vec::new());
        return Ok(());
    }

    let max_index = i64::try_from(usize::MAX).unwrap_or(i64::MAX);
    let ranges = parse_integer_segments(select_file, 1, max_index)
        .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?
        .into_iter()
        .map(|range| -> Result<_> {
            let start = usize::try_from(*range.start()).map_err(|_| {
                Aria2Error::Fatal(FatalError::Config(
                    "select-file index does not fit the current platform".to_string(),
                ))
            })?;
            let end = usize::try_from(*range.end()).map_err(|_| {
                Aria2Error::Fatal(FatalError::Config(
                    "select-file index does not fit the current platform".to_string(),
                ))
            })?;
            Ok(start..=end)
        })
        .collect::<Result<Vec<_>>>()?;

    context.set_file_filter_ranges(&ranges);
    Ok(())
}

pub(super) fn apply_index_out_paths(
    context: &mut crate::download::DownloadContext,
    index_out: Option<&str>,
    dir: &str,
) -> Result<()> {
    let Some(index_out) = index_out else {
        return Ok(());
    };

    for (index, suffix_path) in
        parse_index_out(index_out).map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?
    {
        let path = std::path::Path::new(dir).join(suffix_path);
        context
            .set_file_path_with_index(index, path.to_string_lossy().into_owned())
            .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_file_mapping, file_web_seed_urls, normalized_announce_list, normalized_web_seed_list,
    };
    use crate::download::file_entry::FileEntry;
    use crate::request::request_group::BtFileMapping;

    #[test]
    fn metalink_file_mapping_preserves_torrent_web_seeds_and_appends_mirrors() {
        let torrent_web_seed = "https://torrent-seed.example/file.bin".to_string();
        let metalink_mirror = "https://metalink-mirror.example/file.bin".to_string();
        let mut entry = FileEntry::new("original.bin".into(), 8, 0, vec![torrent_web_seed.clone()]);
        let mapping = BtFileMapping {
            original_name: "original.bin".into(),
            path: "renamed.bin".into(),
            uris: vec![metalink_mirror.clone()],
            max_connection_per_server: 4,
            unique_protocol: false,
        };

        apply_file_mapping(&mut entry, &mapping);

        assert_eq!(entry.path(), "renamed.bin");
        assert_eq!(entry.uris(), vec![torrent_web_seed, metalink_mirror]);
    }

    #[test]
    fn fills_a_missing_tier_from_the_single_announce_field() {
        assert_eq!(
            normalized_announce_list(&[], "https://tracker.example/announce"),
            vec![vec!["https://tracker.example/announce".to_string()]]
        );
    }

    #[test]
    fn keeps_existing_tiers_and_does_not_add_an_empty_announce() {
        let tiers = vec![vec!["https://one.example/announce".to_string()]];
        assert_eq!(normalized_announce_list(&tiers, ""), tiers);
        assert!(normalized_announce_list(&[], "").is_empty());
    }

    #[test]
    fn combines_and_deduplicates_torrent_and_external_web_seeds() {
        assert_eq!(
            normalized_web_seed_list(
                &[
                    "https://seed.test/root/".into(),
                    "https://seed.test/other".into()
                ],
                &[
                    "https://seed.test/root/".into(),
                    "https://extra.test/".into()
                ],
            ),
            vec![
                "https://extra.test/",
                "https://seed.test/other",
                "https://seed.test/root/",
            ]
        );
    }

    #[test]
    fn expands_web_seed_roots_to_single_and_multi_file_paths() {
        let seeds = vec!["https://seed.test/root".to_string()];
        assert_eq!(
            file_web_seed_urls(&seeds, "file name.bin", &[], true),
            vec!["https://seed.test/root".to_string()]
        );
        assert_eq!(
            file_web_seed_urls(
                &seeds,
                "release pack",
                &["sub dir".into(), "a#b.bin".into()],
                false
            ),
            vec!["https://seed.test/root/release%20pack/sub%20dir/a%23b.bin"]
        );
        assert_eq!(
            file_web_seed_urls(
                &["https://seed.test/root/".into()],
                "file name.bin",
                &[],
                true
            ),
            vec!["https://seed.test/root/file%20name.bin"]
        );
    }
}
