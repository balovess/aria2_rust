//! Parameter parsing and wire shaping for option RPC methods.

use std::collections::HashMap;

use crate::backend::BackendRequest;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest};
use crate::rpc_helpers::normalize_rpc_options;

pub(crate) fn parse_get_global_option(_req: &mut JsonRpcRequest) -> BackendRequest {
    BackendRequest::GetGlobalOption
}

pub(crate) fn parse_change_global_option(
    req: &mut JsonRpcRequest,
) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::ChangeGlobalOption {
        options: req.take_param(0)?,
    })
}

pub(crate) fn parse_update_browser_context(
    req: &mut JsonRpcRequest,
) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::UpdateBrowserContext {
        context: req.take_param(0)?,
    })
}

pub(crate) fn parse_clear_browser_context(_req: &mut JsonRpcRequest) -> BackendRequest {
    BackendRequest::ClearBrowserContext
}

pub(crate) fn parse_get_option(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::GetOption {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_change_option(
    req: &mut JsonRpcRequest,
) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::ChangeOption {
        gid: req.take_param(0)?,
        options: req.take_param(1)?,
    })
}

pub(crate) fn normalize_options_response(
    options: HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    serde_json::to_value(normalize_rpc_options(&options)).unwrap_or_else(|_| serde_json::json!({}))
}
