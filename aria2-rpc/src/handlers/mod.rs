//! Pure RPC parameter parsing and response shaping.
//!
//! These modules intentionally do not know how downloads are stored or
//! executed. They turn wire requests into [`BackendRequest`] values; the
//! application-owned backend performs the domain operation.

pub mod bittorrent;
pub mod options;
pub mod status;
pub mod system;
pub mod task;

use crate::backend::{BackendError, BackendEvent};
use crate::json_rpc::{JsonRpcError, JsonRpcRequest};
use crate::websocket::{DownloadEvent, EventType};

/// Parse a GID parameter accepting either one string or a list of strings.
pub(crate) fn parse_gids(
    req: &mut JsonRpcRequest,
    index: usize,
) -> Result<Vec<String>, JsonRpcError> {
    if let Ok(gids) = req.get_param::<Vec<String>>(index) {
        return Ok(gids);
    }
    Ok(vec![req.get_param(index)?])
}

/// Convert a backend-domain error into the wire layer's error taxonomy.
pub(crate) fn backend_error(error: BackendError) -> JsonRpcError {
    match error {
        BackendError::InvalidParams(message) => JsonRpcError::InvalidParams(message),
        BackendError::Execution(message) | BackendError::Unsupported(message) => {
            JsonRpcError::RpcExecution(message)
        }
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
