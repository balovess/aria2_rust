//! RPC handler modules organized by category.
//!
//! - [`task`] — Task management: addUri, remove, pause, unpause, tellStatus, etc.
//! - [`status`] — Status queries: tellActive, tellWaiting, tellStopped, getGlobalStat
//! - [`options`] — Option management: getOption, changeOption, getGlobalOption, changeGlobalOption
//! - [`bittorrent`] — BitTorrent-specific handlers: getPeers, pauseAll, unpauseAll, etc.

pub mod bittorrent;
pub mod options;
pub mod status;
pub mod task;

#[cfg(test)]
mod handler_tests;

use crate::json_rpc::{JsonRpcError, JsonRpcRequest};

/// Parse GID parameter supporting single GID string or array of GIDs.
pub(crate) fn parse_gids(req: &JsonRpcRequest, index: usize) -> Result<Vec<String>, JsonRpcError> {
    if let Ok(gids) = req.get_param::<Vec<String>>(index) {
        return Ok(gids);
    }
    let gid: String = req.get_param(index)?;
    Ok(vec![gid])
}
