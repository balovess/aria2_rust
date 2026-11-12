#[cfg(feature = "bittorrent")]
use aria2_core::download::download_context::{BtFileMode, ContextAttributeType, TorrentAttribute};
use aria2_core::request::request_group::{DownloadStatus, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{
    BackendError, BackendReadSnapshot, BackendResponse, BackendResult, BittorrentInfo,
    BittorrentMetaInfo, GlobalStat, StatusInfo,
};

use super::CoreRpcBackend;

mod bittorrent;
mod files;

use files::{build_file_infos, build_file_infos_from_result};

impl CoreRpcBackend {
    pub(super) fn status_from_group(group: &RequestGroup, gid: &str) -> StatusInfo {
        let snapshot = group.status_snapshot();
        let status = map_status(snapshot.status.clone());
        let bt = snapshot.bt.as_ref();
        let mut info = StatusInfo::new(gid)
            .with_status(status.clone())
            .with_total_length(snapshot.total_length)
            .with_completed_length(snapshot.completed_length)
            .with_upload_length(snapshot.upload_length)
            .with_download_speed(snapshot.download_speed)
            .with_upload_speed(snapshot.upload_speed)
            .with_connections(snapshot.connections)
            .with_http_connection_counts(
                snapshot.http1_connections,
                snapshot.http2_sessions,
                snapshot.http2_streams,
            )
            .with_dir(group.options().dir.clone().unwrap_or_default())
            .with_files(build_file_infos(group, snapshot.completed_length));

        if let Some(http_version) = group.negotiated_http_version() {
            info = info.with_http_version(http_version);
        }

        if let Some(context) = group.get_download_context() {
            info = info
                .with_piece_length(context.get_piece_length() as u64)
                .with_num_pieces(context.get_num_pieces() as u32);
        }

        let followed_by = group.followed_by_gids();
        if !followed_by.is_empty() {
            info = info.with_followed_by(
                followed_by
                    .into_iter()
                    .map(|gid| gid.to_hex_string())
                    .collect(),
            );
        }
        if let Some(following) = group.following_gid() {
            info = info.with_following(following.to_hex_string());
        }
        if let Some(belongs_to) = group.belongs_to_gid() {
            info = info.with_belongs_to(belongs_to.to_hex_string());
        }

        let integrity_man = aria2_core::checksum::check_integrity::man::shared();
        if let Ok(man) = integrity_man.try_read() {
            let (verified_length, pending) = man.status_for_gid(group.gid().value());
            if let Some(length) = verified_length {
                info = info.with_verified_length(length);
            }
            if pending {
                info = info.with_verify_integrity_pending("true");
            }
        }

        if let Some(bt) = bt {
            info = info
                .with_info_hash(bt.info_hash.clone())
                // `numSeeders` is the number of connected seeder peers, not
                // the swarm count reported by a Tracker.
                .with_num_seeders(bt.seeder_count() as u32)
                .with_num_pieces(bt.num_pieces)
                .with_piece_length(bt.piece_length as u64)
                .with_completed_pieces(bt.completed_pieces)
                .with_missing_pieces(bt.missing_pieces)
                .with_seeder(if bt.is_complete() { "true" } else { "false" });
            if let Some(bitfield) = &bt.bitfield {
                info = info.with_bitfield(
                    bitfield
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                );
            }

            #[cfg(feature = "bittorrent")]
            if let Some(context) = group.get_download_context()
                && let Some(attribute) = context
                    .get_attribute(ContextAttributeType::BitTorrent)
                    .and_then(|value| value.downcast_ref::<TorrentAttribute>())
            {
                let mode = match attribute.mode {
                    BtFileMode::Single => "single",
                    BtFileMode::Multi => "multi",
                };
                info = info.with_bittorrent(BittorrentInfo {
                    announce_list: attribute.announce_list.clone(),
                    comment: (!attribute.comment.is_empty()).then(|| attribute.comment.clone()),
                    creation_date: (attribute.creation_date != 0)
                        .then_some(attribute.creation_date),
                    mode: Some(mode.to_string()),
                    info: (!attribute.name.is_empty()).then(|| BittorrentMetaInfo {
                        name: attribute.name.clone(),
                    }),
                });
            }
        }
        info
    }

    pub(super) fn status_from_result(
        result: &aria2_core::request::request_group::DownloadResult,
    ) -> StatusInfo {
        let mut info = StatusInfo::new(result.gid_hex())
            .with_status(map_status(result.status.clone()))
            .with_total_length(result.total_length)
            .with_completed_length(result.completed_length)
            .with_upload_length(result.upload_length)
            .with_download_speed(result.download_speed)
            .with_upload_speed(result.upload_speed)
            .with_error_code(result.code.as_code() as i32)
            .with_error_message(result.message.clone())
            .with_dir(result.dir.clone())
            .with_files(build_file_infos_from_result(result))
            .with_connections(0)
            .with_piece_length(result.piece_length as u64)
            .with_num_pieces(result.num_pieces);

        if !result.followed_by.is_empty() {
            info = info.with_followed_by(
                result
                    .followed_by
                    .iter()
                    .map(|gid| gid.to_hex_string())
                    .collect(),
            );
        }
        if let Some(following) = result.following {
            info = info.with_following(following.to_hex_string());
        }
        if let Some(belongs_to) = result.belongs_to {
            info = info.with_belongs_to(belongs_to.to_hex_string());
        }
        if !result.info_hash.is_empty() {
            info = info
                .with_info_hash(result.info_hash.clone())
                .with_num_seeders(0);
        }
        if result.num_pieces > 0 {
            let completed_pieces =
                Self::completed_pieces_from_bitfield(result.num_pieces, &result.bitfield);
            info = info
                .with_num_pieces(result.num_pieces)
                .with_piece_length(result.piece_length as u64)
                .with_completed_pieces(completed_pieces)
                .with_missing_pieces(result.num_pieces.saturating_sub(completed_pieces));
            if !result.bitfield.is_empty() {
                info = info.with_bitfield(result.bitfield.clone());
            }
        }
        if let Some(metadata) = &result.bt_metadata {
            info = info.with_bittorrent(BittorrentInfo {
                announce_list: metadata.announce_list.clone(),
                comment: metadata.comment.clone(),
                creation_date: metadata.creation_date,
                mode: metadata.mode.clone(),
                info: metadata
                    .name
                    .clone()
                    .map(|name| BittorrentMetaInfo { name }),
            });
        }
        info
    }

    fn completed_pieces_from_bitfield(num_pieces: u32, bitfield: &str) -> u32 {
        bitfield
            .as_bytes()
            .chunks(2)
            .enumerate()
            .map(|(byte_index, pair)| {
                let Ok(pair) = std::str::from_utf8(pair) else {
                    return 0;
                };
                let Ok(byte) = u8::from_str_radix(pair, 16) else {
                    return 0;
                };
                (0..8)
                    .take_while(|bit| byte_index * 8 + bit < num_pieces as usize)
                    .filter(|bit| byte & (0x80 >> bit) != 0)
                    .count() as u32
            })
            .sum()
    }

    pub(super) fn capture_snapshot(&self) -> BackendReadSnapshot {
        let active = self
            .group_man
            .get_active_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect::<Vec<_>>();
        let waiting = self
            .group_man
            .get_waiting_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect::<Vec<_>>();
        let stopped = self
            .group_man
            .get_stopped_results(0, usize::MAX)
            .iter()
            .map(Self::status_from_result)
            .collect::<Vec<_>>();
        let global_stat = global_stat(&active, &waiting, stopped.len());
        BackendReadSnapshot {
            active,
            waiting,
            stopped,
            global_stat,
        }
    }

    pub(super) fn tell_status(&self, gid: String) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        if let Some(group) = self.group_man.group_by_hex(&gid) {
            let group = group.recover();
            return Ok(BackendResult::response(BackendResponse::Status(
                Self::status_from_group(&group, &group.gid().to_hex_string()),
            )));
        }
        if let Some(result) = self.group_man.find_stopped_result(&gid) {
            return Ok(BackendResult::response(BackendResponse::Status(
                Self::status_from_result(&result),
            )));
        }
        Err(Self::execution(format!("No such download for GID#{gid}")))
    }

    pub(super) fn tell_active(&self, keys: Vec<String>) -> Result<BackendResult, BackendError> {
        let statuses = self
            .group_man
            .get_active_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect();
        let _ = keys;
        Ok(BackendResult::response(BackendResponse::Statuses(statuses)))
    }

    pub(super) fn tell_waiting(
        &self,
        offset: i64,
        num: usize,
        keys: Vec<String>,
    ) -> Result<BackendResult, BackendError> {
        let statuses = self
            .group_man
            .get_waiting_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect::<Vec<_>>();
        let _ = keys;
        Ok(BackendResult::response(BackendResponse::Statuses(
            paginate(statuses, offset, num),
        )))
    }

    pub(super) fn tell_stopped(
        &self,
        offset: i64,
        num: usize,
        keys: Vec<String>,
    ) -> Result<BackendResult, BackendError> {
        let statuses = self
            .group_man
            .get_stopped_results(0, usize::MAX)
            .iter()
            .map(Self::status_from_result)
            .collect::<Vec<_>>();
        let _ = keys;
        Ok(BackendResult::response(BackendResponse::Statuses(
            paginate(statuses, offset, num),
        )))
    }

    pub(super) async fn get_option(&self, gid: String) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        if let Some(group) = self.group_man.group_by_hex(&gid) {
            let (snapshot, runtime) = {
                let group = group.recover();
                (group.effective_option_snapshot(), group.runtime_options())
            };
            if let Some(options) = snapshot {
                return Ok(BackendResult::response(BackendResponse::Options(options)));
            }
            if !runtime.is_empty() {
                return Ok(BackendResult::response(BackendResponse::Options(runtime)));
            }
            return Ok(BackendResult::response(BackendResponse::Options(
                self.global_options().await,
            )));
        }
        if let Some(result) = self.group_man.find_stopped_result(&gid) {
            return Ok(BackendResult::response(BackendResponse::Options(
                result.option_snapshot().cloned().unwrap_or_default(),
            )));
        }
        Err(Self::execution(format!("Cannot get option for GID#{gid}")))
    }
}

fn map_status(status: DownloadStatus) -> aria2_rpc::DownloadStatus {
    match status {
        DownloadStatus::Waiting => aria2_rpc::DownloadStatus::Waiting,
        DownloadStatus::Active => aria2_rpc::DownloadStatus::Active,
        DownloadStatus::Paused => aria2_rpc::DownloadStatus::Paused,
        DownloadStatus::Error(message) => aria2_rpc::DownloadStatus::Error(message),
        DownloadStatus::Complete => aria2_rpc::DownloadStatus::Complete,
        DownloadStatus::Removed => aria2_rpc::DownloadStatus::Removed,
    }
}

fn global_stat(active: &[StatusInfo], waiting: &[StatusInfo], stopped: usize) -> GlobalStat {
    GlobalStat {
        download_speed: active
            .iter()
            .chain(waiting)
            .filter_map(|status| status.download_speed)
            .fold(0, u64::saturating_add),
        upload_speed: active
            .iter()
            .chain(waiting)
            .filter_map(|status| status.upload_speed)
            .fold(0, u64::saturating_add),
        num_active: active.len(),
        num_waiting: waiting.len(),
        num_stopped: stopped,
        num_stopped_total: stopped,
    }
}

pub(super) fn paginate<T>(items: Vec<T>, offset: i64, num: usize) -> Vec<T> {
    if num == 0 {
        return Vec::new();
    }
    let size = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let originally_negative = offset < 0;
    let (start, count) = if originally_negative {
        let end = offset.saturating_add(size);
        if end < 0 {
            return Vec::new();
        }
        let count = i64::try_from(num).unwrap_or(i64::MAX);
        let mut start = end.saturating_sub(count.saturating_sub(1));
        let count = if start < 0 {
            start = 0;
            end.saturating_add(1)
        } else {
            count
        };
        (start, count)
    } else {
        if offset >= size {
            return Vec::new();
        }
        (offset, i64::try_from(num).unwrap_or(i64::MAX))
    };
    if start < 0 || start >= size {
        return Vec::new();
    }
    let end = start.saturating_add(count).min(size).max(start);
    let mut selected = items
        .into_iter()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect::<Vec<_>>();
    if originally_negative {
        selected.reverse();
    }
    selected
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
