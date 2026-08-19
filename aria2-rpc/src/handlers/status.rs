//! Parameter parsing and wire shaping for status RPC methods.

use std::collections::HashSet;

use crate::backend::{BackendRequest, BackendResponse};
use crate::json_rpc::{JsonRpcError, JsonRpcRequest};
use crate::types::StatusInfo;

pub(crate) struct StatusKeyFilter {
    keys: HashSet<String>,
}

pub(crate) fn status_key_filter(keys: &[String]) -> Option<StatusKeyFilter> {
    (!keys.is_empty()).then(|| StatusKeyFilter {
        keys: keys.iter().cloned().collect(),
    })
}

pub(crate) fn parse_tell_active(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::TellActive {
        keys: req
            .take_optional_param::<Vec<String>>(0)?
            .unwrap_or_default(),
    })
}

fn parse_pagination(req: &mut JsonRpcRequest) -> Result<(i64, usize, Vec<String>), JsonRpcError> {
    let offset = req.take_param(0)?;
    let num = req.take_param::<i64>(1)?;
    if num < 0 {
        return Err(JsonRpcError::RpcExecution(
            "num must be greater than or equal to 0".into(),
        ));
    }
    let num = usize::try_from(num)
        .map_err(|_| JsonRpcError::RpcExecution("num is out of range".into()))?;
    let keys = req
        .take_optional_param::<Vec<String>>(2)?
        .unwrap_or_default();
    Ok((offset, num, keys))
}

pub(crate) fn parse_tell_waiting(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let (offset, num, keys) = parse_pagination(req)?;
    Ok(BackendRequest::TellWaiting { offset, num, keys })
}

pub(crate) fn parse_tell_stopped(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let (offset, num, keys) = parse_pagination(req)?;
    Ok(BackendRequest::TellStopped { offset, num, keys })
}

/// Apply the optional `keys` projection after the backend has built a
/// semantically complete DTO. This keeps the projection policy in the wire
/// layer and lets every backend use the same public shape.
pub(crate) fn serialize_status_response(
    response: BackendResponse,
    keys: &[String],
) -> Result<serde_json::Value, JsonRpcError> {
    let filter = status_key_filter(keys);
    match response {
        BackendResponse::Status(status) => serialize_status(status, filter.as_ref()),
        BackendResponse::Statuses(statuses) => statuses
            .into_iter()
            .map(|status| serialize_status(status, filter.as_ref()))
            .collect(),
        other => other.into_json_value().map_err(super::backend_error),
    }
}

fn serialize_status(
    status: StatusInfo,
    filter: Option<&StatusKeyFilter>,
) -> Result<serde_json::Value, JsonRpcError> {
    let mut value = serde_json::to_value(status)
        .map_err(|error| JsonRpcError::InternalError(format!("Serialization failed: {error}")))?;
    if let Some(filter) = filter
        && let Some(fields) = value.as_object_mut()
    {
        fields.retain(|key, _| filter.keys.contains(key));
    }
    Ok(value)
}
