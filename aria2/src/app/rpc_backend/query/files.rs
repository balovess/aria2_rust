use std::path::PathBuf;

use aria2_core::request::request_group::{DownloadStatus, RequestGroup};
use aria2_core::segment::piece_storage::BitfieldMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{
    BackendError, BackendResponse, BackendResult, FileInfo, ServerInfo, ServerInfoIndex, UriEntry,
    UriStatus,
};

use super::CoreRpcBackend;

impl CoreRpcBackend {
    pub(in crate::app::rpc_backend) fn get_uris(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        let group = self
            .group_man
            .group_by_hex(&gid)
            .ok_or_else(|| Self::execution(format!("No URI data is available for GID#{gid}")))?;
        let group = group.recover();
        // Match aria2_original's GetUrisRpcMethod: it delegates to
        // createUriEntry() for the first FileEntry only, rather than
        // flattening every file in a multi-file download.
        let entries = group
            .get_download_context()
            .and_then(|context| context.get_file_entries().first().map(build_uri_entries))
            .unwrap_or_else(|| group.uris().iter().cloned().map(UriEntry::new).collect());
        Ok(BackendResult::response(BackendResponse::Uris(entries)))
    }

    pub(in crate::app::rpc_backend) fn get_files(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        if let Some(group) = self.group_man.group_by_hex(&gid) {
            let group = group.recover();
            return Ok(BackendResult::response(BackendResponse::Files(
                build_file_infos(&group, group.get_completed_length()),
            )));
        }
        if let Some(result) = self.group_man.find_stopped_result(&gid) {
            return Ok(BackendResult::response(BackendResponse::Files(
                build_file_infos_from_result(&result),
            )));
        }
        Err(Self::execution(format!(
            "No file data is available for GID#{gid}"
        )))
    }

    pub(in crate::app::rpc_backend) fn get_servers(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        let group = self
            .group_man
            .group_by_hex(&gid)
            .ok_or_else(|| Self::execution(format!("No active download for GID#{gid}")))?;
        let group = group.recover();
        if !matches!(group.status(), DownloadStatus::Active) {
            return Err(Self::execution(format!("No active download for GID#{gid}")));
        }
        let servers = group
            .get_download_context()
            .map(|context| {
                context
                    .get_file_entries()
                    .iter()
                    .enumerate()
                    .map(|(index, file)| ServerInfoIndex {
                        index: index + 1,
                        servers: file
                            .in_flight_requests()
                            .iter()
                            .filter_map(|request| {
                                let stats = request.peer_stat()?;
                                Some(
                                    ServerInfo::new(request.uri())
                                        .with_current_uri(request.current_uri())
                                        .with_download_speed(stats.download_speed),
                                )
                            })
                            .collect(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(BackendResult::response(BackendResponse::Servers(servers)))
    }
}

pub(super) fn build_file_infos(group: &RequestGroup, completed: u64) -> Vec<FileInfo> {
    let resolved_path = group.resolved_output_path();
    let fallback_path = || {
        if let Some(path) = &resolved_path {
            return path.clone();
        }
        let name = group
            .options()
            .out
            .clone()
            .or_else(|| group.output_name())
            .or_else(|| {
                group
                    .uris()
                    .first()
                    .map(|uri| aria2_core::validation::uri::sanitize_filename_from_uri(uri))
            })
            .unwrap_or_default();
        match group.options().dir.as_deref().filter(|dir| !dir.is_empty()) {
            Some(dir) if !name.is_empty() => PathBuf::from(dir).join(name).to_string_lossy().into(),
            _ => name,
        }
    };

    if let Some(context) = group.get_download_context() {
        let completion = bt_completion_bitfield(group);
        return context
            .get_file_entries()
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let path = if index == 0 {
                    resolved_path.clone().unwrap_or_else(|| {
                        if file.path().is_empty() {
                            fallback_path()
                        } else {
                            file.path().to_owned()
                        }
                    })
                } else if file.path().is_empty() {
                    fallback_path()
                } else {
                    file.path().to_owned()
                };
                let mut info = FileInfo::new(path, file.length())
                    .with_index(index + 1)
                    .with_completed(completed_length_for_file(
                        completion.as_ref(),
                        completed,
                        file.offset(),
                        file.length(),
                    ))
                    .with_uris(build_uri_entries(file));
                info.selected = file.is_requested();
                info
            })
            .collect();
    }

    let mut info = FileInfo::new(fallback_path(), group.get_total_length_atomic())
        .with_index(1)
        .with_completed(completed)
        .with_uris(group.uris().iter().cloned().map(UriEntry::new).collect());
    info.selected = true;
    vec![info]
}

fn completed_length_for_file(
    completion: Option<&BitfieldMan>,
    total_completed: u64,
    offset: u64,
    length: u64,
) -> u64 {
    completion
        .map(|bitfield| bitfield.get_offset_completed_length(offset, length))
        .unwrap_or_else(|| total_completed.saturating_sub(offset).min(length))
}

fn bt_completion_bitfield(group: &RequestGroup) -> Option<BitfieldMan> {
    let piece_length = group.get_bt_piece_length() as u64;
    let bitfield = group.get_bt_bitfield()?;
    if piece_length == 0 || bitfield.is_empty() {
        return None;
    }
    let mut completion = BitfieldMan::new(piece_length, group.get_total_length_atomic());
    completion.set_bitfield(&bitfield);
    Some(completion)
}

fn build_uri_entries(file: &aria2_core::download::file_entry::FileEntry) -> Vec<UriEntry> {
    let remaining = file.remaining_uris();
    file.uris()
        .into_iter()
        .map(|uri| UriEntry {
            status: if remaining.iter().any(|candidate| candidate == &uri) {
                UriStatus::Waiting
            } else {
                UriStatus::Used
            },
            uri,
        })
        .collect()
}

pub(super) fn build_file_infos_from_result(
    result: &aria2_core::request::request_group::DownloadResult,
) -> Vec<FileInfo> {
    result
        .files
        .iter()
        .map(|file| {
            let mut info = FileInfo::new(file.path.clone(), file.length)
                .with_index(file.index)
                .with_completed(file.completed_length)
                .with_uris(
                    file.uris
                        .iter()
                        .map(|uri| UriEntry {
                            uri: uri.uri.clone(),
                            status: match uri.status.as_str() {
                                "used" | "spent" => UriStatus::Used,
                                _ => UriStatus::Waiting,
                            },
                        })
                        .collect(),
                );
            info.selected = file.selected;
            info
        })
        .collect()
}
