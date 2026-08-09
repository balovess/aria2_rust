//! System RPC handlers for method discovery and notification listing.
//!
//! Implements `system.listMethods` and `system.listNotifications` from the
//! aria2 RPC specification.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

impl RpcEngine {
    /// Return the RPC method catalog in the same order as aria2's
    /// `RpcMethodFactory::allMethodNames()`.
    pub(crate) fn rpc_method_names() -> Vec<&'static str> {
        let mut methods = vec!["aria2.addUri"];

        #[cfg(feature = "bittorrent")]
        methods.extend(["aria2.addTorrent", "aria2.getPeers"]);

        #[cfg(feature = "metalink")]
        methods.push("aria2.addMetalink");

        methods.extend([
            "aria2.remove",
            "aria2.pause",
            "aria2.forcePause",
            "aria2.pauseAll",
            "aria2.forcePauseAll",
            "aria2.unpause",
            "aria2.unpauseAll",
            "aria2.forceRemove",
            "aria2.changePosition",
            "aria2.tellStatus",
            "aria2.getUris",
            "aria2.getFiles",
            "aria2.getServers",
            "aria2.tellActive",
            "aria2.tellWaiting",
            "aria2.tellStopped",
            "aria2.getOption",
            "aria2.changeUri",
            "aria2.changeOption",
            "aria2.getGlobalOption",
            "aria2.changeGlobalOption",
            "aria2.purgeDownloadResult",
            "aria2.removeDownloadResult",
            "aria2.getVersion",
            "aria2.getSessionInfo",
            "aria2.shutdown",
            "aria2.forceShutdown",
            "aria2.getGlobalStat",
            "aria2.saveSession",
            "system.multicall",
            "system.listMethods",
            "system.listNotifications",
        ]);

        methods
    }

    /// Return the notification catalog in the same order as aria2's
    /// `RpcMethodFactory::allNotificationsNames()`.
    pub(crate) fn rpc_notification_names() -> Vec<&'static str> {
        const CORE_NOTIFICATIONS: [&str; 5] = [
            "aria2.onDownloadStart",
            "aria2.onDownloadPause",
            "aria2.onDownloadStop",
            "aria2.onDownloadComplete",
            "aria2.onDownloadError",
        ];

        #[cfg(feature = "bittorrent")]
        {
            let mut notifications = CORE_NOTIFICATIONS.to_vec();
            notifications.push("aria2.onBtDownloadComplete");
            notifications
        }

        #[cfg(not(feature = "bittorrent"))]
        CORE_NOTIFICATIONS.to_vec()
    }

    /// Handle `system.listMethods` - Return all supported RPC methods.
    ///
    /// Returns an array of method names that this RPC server supports.
    /// This is useful for clients to discover available functionality.
    pub async fn handle_list_methods(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let methods = Self::rpc_method_names();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            methods,
        ))
    }

    /// Handle `system.listNotifications` - Return all supported event notifications.
    ///
    /// Returns the feature-specific notification names advertised by aria2.
    pub async fn handle_list_notifications(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let notifications = Self::rpc_notification_names();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            notifications,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_rpc::JsonRpcRequest;

    #[tokio::test]
    async fn test_list_methods() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
        let expected: Vec<String> = RpcEngine::rpc_method_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(methods, expected);
    }

    #[tokio::test]
    async fn test_list_notifications() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("system.listNotifications", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
        let expected: Vec<String> = RpcEngine::rpc_notification_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(notifications, expected);
    }
}
