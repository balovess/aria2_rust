use std::collections::HashMap;

use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::request::request_group_man::ChangePositionMode;
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{BackendError, BackendEvent, BackendResponse, BackendResult, PositionMode};

use super::CoreRpcBackend;
use super::values::normalize_options;

impl CoreRpcBackend {
    pub(super) fn change_position(
        &self,
        gid: &str,
        position: i32,
        mode: PositionMode,
    ) -> Result<BackendResult, BackendError> {
        let gid = Self::parse_gid(gid)?;
        let mode = match mode {
            PositionMode::SetFromStart => ChangePositionMode::SetFromStart,
            PositionMode::MoveFromStart => ChangePositionMode::MoveFromStart,
            PositionMode::SetFromEnd => ChangePositionMode::SetFromEnd,
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
        let parsed = Self::parse_gid(&gid)?;
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
            BackendResponse::Gid(gid.clone()),
            vec![BackendEvent::DownloadPause(gid)],
        ))
    }

    pub(super) fn unpause(&self, gid: String) -> Result<BackendResult, BackendError> {
        let parsed = Self::parse_gid(&gid)?;
        self.group_man
            .unpause_group(parsed)
            .map_err(|error| Self::execution(error.to_string()))?;
        self.send(EngineCommand::Unpause { gid: parsed })?;
        Ok(BackendResult::with_events(
            BackendResponse::Gid(gid.clone()),
            vec![BackendEvent::DownloadStart(gid)],
        ))
    }

    pub(super) fn remove(&self, gid: String, force: bool) -> Result<BackendResult, BackendError> {
        let parsed = Self::parse_gid(&gid)?;
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
            BackendResponse::Gid(gid.clone()),
            vec![BackendEvent::DownloadStop(gid)],
        ))
    }

    pub(super) fn change_option(
        &self,
        gid: String,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<BackendResult, BackendError> {
        self.group_man
            .change_group_options(&gid, normalize_options(&options))
            .map_err(Self::execution)?;
        Ok(BackendResult::response(BackendResponse::Text("OK".into())))
    }
}
