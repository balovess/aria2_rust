use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::request::request_group_man::PositionMode as CorePositionMode;
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{BackendError, BackendEvent, BackendResponse, BackendResult, PositionMode};

use super::CoreRpcBackend;
use super::values::normalize_options;

impl CoreRpcBackend {
    pub(super) async fn remove_download_files(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let root_gid = self.parse_gid(&gid)?;
        let mut pending = VecDeque::from([root_gid]);
        let mut visited = HashSet::new();
        let mut seen_paths = HashSet::<PathBuf>::new();
        let mut paths = Vec::new();

        while let Some(current_gid) = pending.pop_front() {
            if !visited.insert(current_gid) {
                continue;
            }
            if self.group_man.find_group(current_gid).is_some() {
                return Err(Self::execution(format!(
                    "Cannot remove files for non-stopped GID#{}",
                    current_gid.to_hex_string()
                )));
            }
            let current_hex = current_gid.to_hex_string();
            let result = self
                .group_man
                .find_stopped_result(&current_hex)
                .ok_or_else(|| {
                    Self::execution(format!("No retained download result for GID#{current_hex}"))
                })?;

            if !result.in_memory_download {
                for file in result.files.iter().filter(|file| file.selected) {
                    let path = PathBuf::from(&file.path);
                    if !path.as_os_str().is_empty() && seen_paths.insert(path.clone()) {
                        paths.push(path);
                    }
                }
            }
            pending.extend(result.followed_by);
        }

        for path in paths {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Self::execution(format!(
                        "Could not remove downloaded file '{}': {error}",
                        path.display()
                    )));
                }
            }
        }

        Ok(BackendResult::response(BackendResponse::Text("OK".into())))
    }

    pub(super) fn change_position(
        &self,
        gid: &str,
        position: i32,
        mode: PositionMode,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(gid)?;
        let mode = match mode {
            PositionMode::SetFromStart => CorePositionMode::SetFromStart,
            PositionMode::MoveFromStart => CorePositionMode::MoveFromStart,
            PositionMode::SetFromEnd => CorePositionMode::SetFromEnd,
        };
        let position = self
            .group_man
            .change_position(gid, position, mode)
            .map_err(|error| Self::execution(error.to_string()))?;
        Ok(BackendResult::response(BackendResponse::Position(position)))
    }

    pub(super) fn lifecycle_gids(&self) -> Vec<String> {
        self.group_man
            .all_groups()
            .into_iter()
            .map(|(_, group)| group.recover().gid().to_hex_string())
            .collect()
    }

    pub(super) fn pause(&self, gid: String, force: bool) -> Result<BackendResult, BackendError> {
        let parsed = self.parse_gid(&gid)?;
        let canonical_gid = parsed.to_hex_string();
        if force {
            self.group_man
                .force_pause_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.send(EngineCommand::ForcePause { gid: parsed })?;
        } else {
            self.group_man
                .pause_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.send(EngineCommand::Pause { gid: parsed })?;
        }
        Ok(BackendResult::with_events(
            BackendResponse::Gid(canonical_gid.clone()),
            vec![BackendEvent::DownloadPause(canonical_gid)],
        ))
    }

    pub(super) fn unpause(&self, gid: String) -> Result<BackendResult, BackendError> {
        let parsed = self.parse_gid(&gid)?;
        let canonical_gid = parsed.to_hex_string();
        self.group_man
            .unpause_group(parsed)
            .map_err(|error| Self::execution(error.to_string()))?;
        self.send(EngineCommand::Unpause { gid: parsed })?;
        Ok(BackendResult::with_events(
            BackendResponse::Gid(canonical_gid.clone()),
            vec![BackendEvent::DownloadStart(canonical_gid)],
        ))
    }

    pub(super) fn remove(&self, gid: String, force: bool) -> Result<BackendResult, BackendError> {
        let parsed = self.parse_gid(&gid)?;
        let canonical_gid = parsed.to_hex_string();
        let enqueue = if force {
            self.group_man
                .force_remove_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.group_man.find_group(parsed).is_some()
        } else {
            self.group_man
                .remove_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.group_man.find_group(parsed).is_some()
        };
        if enqueue {
            self.send(if force {
                EngineCommand::ForceRemoveDownload { gid: parsed }
            } else {
                EngineCommand::RemoveDownload { gid: parsed }
            })?;
        }
        Ok(BackendResult::with_events(
            BackendResponse::Gid(canonical_gid.clone()),
            vec![BackendEvent::DownloadStop(canonical_gid)],
        ))
    }

    pub(super) fn change_option(
        &self,
        gid: String,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        self.group_man
            .change_group_options(&gid, normalize_options(&options))
            .map_err(Self::execution)?;
        Ok(BackendResult::response(BackendResponse::Text("OK".into())))
    }
}
