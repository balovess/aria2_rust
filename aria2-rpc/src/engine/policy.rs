pub(crate) fn rpc_method_requires_auth(method: &str) -> bool {
    !matches!(method, "system.listMethods" | "system.listNotifications")
}

pub(crate) fn rpc_method_is_read_only(method: &str) -> bool {
    matches!(
        method,
        "aria2.tellStatus"
            | "aria2.tellActive"
            | "aria2.tellWaiting"
            | "aria2.tellStopped"
            | "aria2.getGlobalStat"
            | "aria2.getUris"
            | "aria2.getFiles"
            | "aria2.getServers"
            | "aria2.getGlobalOption"
            | "aria2.getOption"
            | "aria2.getPeers"
            | "aria2.getPeerStats"
            | "aria2.getPeerDetails"
            | "aria2.getVersion"
            | "aria2.getSessionInfo"
            | "system.listMethods"
            | "system.listNotifications"
    )
}

pub(crate) fn rpc_method_is_mutating(method: &str) -> bool {
    matches!(
        method,
        "aria2.addUri"
            | "aria2.addTorrent"
            | "aria2.addMetalink"
            | "aria2.remove"
            | "aria2.pause"
            | "aria2.forcePause"
            | "aria2.unpause"
            | "aria2.purgeDownloadResult"
            | "aria2.removeDownloadResult"
            | "aria2.removeDownloadFiles"
            | "aria2.changeGlobalOption"
            | "aria2.changeOption"
            | "aria2.pauseAll"
            | "aria2.forcePauseAll"
            | "aria2.unpauseAll"
            | "aria2.changeUri"
            | "aria2.saveSession"
            | "aria2.changePosition"
            | "aria2.forceRemove"
            | "aria2.shutdown"
            | "aria2.forceShutdown"
            | "aria2.updateBrowserContext"
            | "aria2.clearBrowserContext"
    )
}

pub(crate) fn rpc_method_uses_read_snapshot(method: &str) -> bool {
    matches!(
        method,
        "aria2.tellActive" | "aria2.tellWaiting" | "aria2.tellStopped" | "aria2.getGlobalStat"
    )
}

pub(super) fn multicall_requires_mutation(params: &serde_json::Value) -> bool {
    let Some(calls) = params
        .as_array()
        .and_then(|params| params.first())
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    calls.iter().any(|call| {
        call.get("methodName")
            .and_then(serde_json::Value::as_str)
            .is_none_or(rpc_method_is_mutating)
    })
}
