//! Parameter parsing for optional and multi-task RPC methods.

use crate::backend::BackendRequest;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest};

pub(crate) fn parse_get_peers(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::GetPeers {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_get_uris(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::GetUris {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_get_files(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::GetFiles {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_get_servers(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::GetServers {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_remove_download_result(
    req: &mut JsonRpcRequest,
) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::RemoveDownloadResult {
        gid: req.take_param(0)?,
    })
}

pub(crate) fn parse_pause_all(_req: &mut JsonRpcRequest) -> BackendRequest {
    BackendRequest::PauseAll
}

pub(crate) fn parse_force_pause_all(_req: &mut JsonRpcRequest) -> BackendRequest {
    BackendRequest::ForcePauseAll
}

pub(crate) fn parse_unpause_all(_req: &mut JsonRpcRequest) -> BackendRequest {
    BackendRequest::UnpauseAll
}

pub(crate) fn parse_purge_download_result(_req: &mut JsonRpcRequest) -> BackendRequest {
    BackendRequest::PurgeDownloadResult
}
