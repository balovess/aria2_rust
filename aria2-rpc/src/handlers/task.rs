//! Parameter parsing for task and lifecycle RPC methods.

use std::collections::HashMap;

use crate::backend::{BackendRequest, PositionMode};
use crate::json_rpc::{JsonRpcError, JsonRpcRequest};

fn optional_position(
    req: &mut JsonRpcRequest,
    index: usize,
) -> Result<Option<usize>, JsonRpcError> {
    let Some(position) = req.take_optional_param::<i64>(index)? else {
        return Ok(None);
    };
    if position < 0 {
        return Err(JsonRpcError::RpcExecution(
            "Position must be greater than or equal to 0.".into(),
        ));
    }
    usize::try_from(position)
        .map(Some)
        .map_err(|_| JsonRpcError::RpcExecution("Position is out of range.".into()))
}

fn decode_rpc_payload(input: String) -> Result<Vec<u8>, JsonRpcError> {
    let encoded = input
        .strip_prefix("data:")
        .and_then(|value| value.split_once(',').map(|(_, payload)| payload))
        .unwrap_or(&input);
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .map_err(|error| JsonRpcError::InvalidParams(format!("base64 decode failed: {error}")))
}

pub(crate) fn parse_add_uri(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let uris = req.take_param::<Vec<String>>(0)?;
    let options = req
        .take_optional_param::<HashMap<String, serde_json::Value>>(1)?
        .unwrap_or_default();
    let position = optional_position(req, 2)?;
    Ok(BackendRequest::AddUri {
        uris,
        options,
        position,
    })
}

pub(crate) fn parse_add_torrent(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let encoded = req.take_param::<String>(0)?;
    let additional_uris = req
        .take_optional_param::<Vec<String>>(1)?
        .unwrap_or_default();
    let options = req
        .take_optional_param::<HashMap<String, serde_json::Value>>(2)?
        .unwrap_or_default();
    let position = optional_position(req, 3)?;
    let data = decode_rpc_payload(encoded)?;
    // Torrent syntax is domain data; the backend owns parsing and validation.
    Ok(BackendRequest::AddTorrent {
        data,
        additional_uris,
        options,
        position,
    })
}

pub(crate) fn parse_add_metalink(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let encoded = req.take_param::<String>(0)?;
    let options = req
        .take_optional_param::<HashMap<String, serde_json::Value>>(1)?
        .unwrap_or_default();
    let position = optional_position(req, 2)?;
    let data = decode_rpc_payload(encoded)?;
    // Metalink syntax is domain data; the backend owns parsing and validation.
    Ok(BackendRequest::AddMetalink {
        data,
        options,
        position,
    })
}

pub(crate) fn parse_remove(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::Remove {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_pause(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::Pause {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_force_pause(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::ForcePause {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_unpause(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::Unpause {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_tell_status(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let gid = req.take_param(0)?;
    let keys = req
        .take_optional_param::<Vec<String>>(1)?
        .unwrap_or_default();
    Ok(BackendRequest::TellStatus { gid, keys })
}

pub(crate) fn parse_force_remove(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::ForceRemove {
        gids: super::parse_gids(req, 0)?,
    })
}

pub(crate) fn parse_change_uri(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let gid = req.take_param(0)?;
    let file_index = req.take_param::<i64>(1)?;
    if file_index < 1 {
        return Err(JsonRpcError::InvalidParams(
            "fileIndex must be at least 1".into(),
        ));
    }
    let delete_uris = req.take_param(2)?;
    let add_uris = req.take_param(3)?;
    let position = optional_position(req, 4)?;
    Ok(BackendRequest::ChangeUri {
        gid,
        file_index: usize::try_from(file_index)
            .map_err(|_| JsonRpcError::InvalidParams("fileIndex is out of range".into()))?,
        delete_uris,
        add_uris,
        position,
    })
}

pub(crate) fn parse_change_position(
    req: &mut JsonRpcRequest,
) -> Result<BackendRequest, JsonRpcError> {
    let gid = req.take_param(0)?;
    let position = req.take_param(1)?;
    let mode = match req.take_param::<String>(2)?.as_str() {
        "POS_SET" => PositionMode::SetFromStart,
        "POS_CUR" => PositionMode::MoveFromStart,
        "POS_END" => PositionMode::SetFromEnd,
        _ => return Err(JsonRpcError::InvalidParams("Invalid position mode".into())),
    };
    Ok(BackendRequest::ChangePosition {
        gid,
        position,
        mode,
    })
}

pub(crate) fn parse_save_session(_req: &mut JsonRpcRequest) -> BackendRequest {
    BackendRequest::SaveSession
}

pub(crate) fn parse_shutdown(_req: &mut JsonRpcRequest, force: bool) -> BackendRequest {
    BackendRequest::Shutdown { force }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    #[test]
    fn add_torrent_parser_leaves_content_validation_to_backend() {
        let data = b"not a torrent document".to_vec();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let mut request = JsonRpcRequest::new("aria2.addTorrent", json!([encoded]));

        let parsed = parse_add_torrent(&mut request).expect("payload should be decoded");
        assert!(matches!(
            parsed,
            BackendRequest::AddTorrent { data: parsed, .. } if parsed == data
        ));
    }

    #[test]
    fn add_metalink_parser_leaves_content_validation_to_backend() {
        let data = b"not an XML document".to_vec();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let mut request = JsonRpcRequest::new("aria2.addMetalink", json!([encoded]));

        let parsed = parse_add_metalink(&mut request).expect("payload should be decoded");
        assert!(matches!(
            parsed,
            BackendRequest::AddMetalink { data: parsed, .. } if parsed == data
        ));
    }
}
