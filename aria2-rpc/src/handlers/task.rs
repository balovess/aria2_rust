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

fn decode_rpc_payload(input: &str) -> Vec<u8> {
    let encoded = input
        .strip_prefix("data:")
        .and_then(|value| value.split_once(',').map(|(_, payload)| payload))
        .unwrap_or(input);
    crate::rpc_helpers::decode_aria2_base64(encoded)
}

pub(crate) fn parse_add_uri(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let uris = super::take_string_list(req, 0)?;
    if uris.is_empty() {
        return Err(JsonRpcError::RpcExecution("URI is not provided.".into()));
    }
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
    let additional_uris = super::take_optional_string_list(req, 1)?;
    let options = req
        .take_optional_param::<HashMap<String, serde_json::Value>>(2)?
        .unwrap_or_default();
    let position = optional_position(req, 3)?;
    let data = decode_rpc_payload(&encoded);
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
    let data = decode_rpc_payload(&encoded);
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
    let keys = super::take_optional_string_list(req, 1)?;
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
        return Err(JsonRpcError::RpcExecution(
            "The integer parameter at 1 has invalid value: the value must be greater than or equal to 1."
                .into(),
        ));
    }
    let delete_uris = super::take_string_list(req, 2)?;
    let add_uris = super::take_string_list(req, 3)?;
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
        _ => return Err(JsonRpcError::RpcExecution("Illegal argument.".into())),
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

    #[test]
    fn json_payload_uses_aria2_permissive_base64_rules() {
        let mut request =
            JsonRpcRequest::new("aria2.addTorrent", json!(["!!bm90IGEgdG9ycmVudA=="]));
        let parsed = parse_add_torrent(&mut request).expect("payload should decode");
        assert!(matches!(
            parsed,
            BackendRequest::AddTorrent { data, .. } if data == b"not a torrent"
        ));
    }

    #[test]
    fn uri_lists_ignore_non_strings_and_empty_lists_fail() {
        let mut request = JsonRpcRequest::new(
            "aria2.addUri",
            json!([["https://example.test/file", 42, null]]),
        );
        let parsed = parse_add_uri(&mut request).expect("string entries should remain");
        assert!(matches!(
            parsed,
            BackendRequest::AddUri { uris, .. }
                if uris == ["https://example.test/file".to_string()]
        ));

        let mut empty = JsonRpcRequest::new("aria2.addUri", json!([[42, false]]));
        let error = parse_add_uri(&mut empty).unwrap_err();
        assert_eq!(error.code(), 1);
        assert_eq!(error.message(), "URI is not provided.");
    }
}
