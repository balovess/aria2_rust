//! Pure RPC parameter parsing and response shaping.
//!
//! These modules intentionally do not know how downloads are stored or
//! executed. They turn wire requests into [`BackendRequest`] values; the
//! application-owned backend performs the domain operation.

pub mod bittorrent;
pub mod options;
pub mod status;
pub mod task;

use crate::backend::{BackendError, BackendEvent};
use crate::json_rpc::{JsonRpcError, JsonRpcRequest};
use crate::websocket::{DownloadEvent, EventType};

pub(crate) fn take_string_list(
    req: &mut JsonRpcRequest,
    index: usize,
) -> Result<Vec<String>, JsonRpcError> {
    let values = req.take_param::<Vec<serde_json::Value>>(index)?;
    Ok(values
        .into_iter()
        .filter_map(|value| match value {
            serde_json::Value::String(value) => Some(value),
            _ => None,
        })
        .collect())
}

pub(crate) fn take_optional_string_list(
    req: &mut JsonRpcRequest,
    index: usize,
) -> Result<Vec<String>, JsonRpcError> {
    let values = req
        .take_optional_param::<Vec<serde_json::Value>>(index)?
        .unwrap_or_default();
    Ok(values
        .into_iter()
        .filter_map(|value| match value {
            serde_json::Value::String(value) => Some(value),
            _ => None,
        })
        .collect())
}

/// Parse a GID parameter accepting either one string or a list of strings.
pub(crate) fn parse_gids(
    req: &mut JsonRpcRequest,
    index: usize,
) -> Result<Vec<String>, JsonRpcError> {
    match req.optional_param_value(index) {
        Some(serde_json::Value::Array(_)) => take_string_list(req, index),
        Some(serde_json::Value::String(_)) => Ok(vec![req.take_param(index)?]),
        Some(_) => Err(JsonRpcError::RpcExecution(format!(
            "The parameter at {index} has wrong type."
        ))),
        None => Err(JsonRpcError::RpcExecution(format!(
            "The parameter at {index} is required but missing."
        ))),
    }
}

/// Convert a backend-domain error into the wire layer's error taxonomy.
pub(crate) fn backend_error(error: BackendError) -> JsonRpcError {
    match error {
        BackendError::InvalidParams(message)
        | BackendError::Execution(message)
        | BackendError::Unsupported(message) => JsonRpcError::RpcExecution(message),
        BackendError::Internal(message) => JsonRpcError::InternalError(message),
    }
}

/// Convert backend lifecycle effects into the notification objects owned by
/// the RPC transport.
pub(crate) fn event_notification(event: BackendEvent) -> (EventType, DownloadEvent) {
    match event {
        BackendEvent::DownloadStart(gid) => {
            (EventType::DownloadStart, DownloadEvent::download_start(gid))
        }
        BackendEvent::DownloadPause(gid) => {
            (EventType::DownloadPause, DownloadEvent::download_pause(gid))
        }
        BackendEvent::DownloadStop(gid) => {
            (EventType::DownloadStop, DownloadEvent::download_stop(gid))
        }
    }
}
