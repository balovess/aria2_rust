//! System RPC handlers for method discovery and notification listing.
//!
//! Implements `system.listMethods` and `system.listNotifications` from the
//! aria2 RPC specification.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

impl RpcEngine {
    /// Handle `system.listMethods` - Return all supported RPC methods.
    ///
    /// Returns an array of method names that this RPC server supports.
    /// This is useful for clients to discover available functionality.
    pub async fn handle_list_methods(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let methods = vec![
            // Task management
            "aria2.addUri",
            "aria2.addTorrent",
            "aria2.addMetalink",
            "aria2.remove",
            "aria2.forceRemove",
            "aria2.pause",
            "aria2.pauseAll",
            "aria2.forcePause",
            "aria2.forcePauseAll",
            "aria2.unpause",
            "aria2.unpauseAll",
            // Status queries
            "aria2.tellStatus",
            "aria2.tellActive",
            "aria2.tellWaiting",
            "aria2.tellStopped",
            "aria2.getGlobalStat",
            // Options
            "aria2.getOption",
            "aria2.changeOption",
            "aria2.getGlobalOption",
            "aria2.changeGlobalOption",
            // Position/URI management
            "aria2.changePosition",
            "aria2.changeUri",
            // Session/System
            "aria2.getVersion",
            "aria2.getSessionInfo",
            "aria2.saveSession",
            "aria2.shutdown",
            "aria2.forceShutdown",
            // Results management
            "aria2.purgeDownloadResult",
            "aria2.removeDownloadResult",
            // BitTorrent specific
            "aria2.getPeers",
            "aria2.getUris",
            "aria2.getFiles",
            "aria2.getServers",
            // System methods
            "system.multicall",
            "system.listMethods",
            "system.listNotifications",
        ];
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            methods,
        ))
    }

    /// Handle `system.listNotifications` - Return all supported event notifications.
    ///
    /// Returns an array of notification event names that can be sent via WebSocket.
    /// These events are broadcast when download state changes occur.
    ///
    /// # Compatibility with original aria2
    ///
    /// Matches `RpcMethodFactory.cc::rpcNotificationsNames` exactly — 6 events
    /// when BitTorrent is enabled (always-on for aria2-rust), 5 otherwise.
    /// The original aria2 does NOT advertise `aria2.onBtDownloadError`;
    /// BT download errors emit the generic `aria2.onDownloadError` instead.
    /// Although `EventType::BtDownloadError` exists as a non-standard extension
    /// for callers that want to emit it explicitly, it is intentionally
    /// absent from this list to preserve plugin compatibility (e.g., AriaNg
    /// validates the response against the documented 6-event set).
    pub async fn handle_list_notifications(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let notifications = vec![
            "aria2.onDownloadStart",
            "aria2.onDownloadPause",
            "aria2.onDownloadStop",
            "aria2.onDownloadComplete",
            "aria2.onDownloadError",
            "aria2.onBtDownloadComplete",
        ];
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
        assert_eq!(methods.len(), 36);
        assert!(methods.contains(&"aria2.addUri".to_string()));
        assert!(methods.contains(&"aria2.shutdown".to_string()));
        assert!(methods.contains(&"aria2.forceShutdown".to_string()));
        assert!(methods.contains(&"system.listMethods".to_string()));
        assert!(methods.contains(&"system.listNotifications".to_string()));
    }

    #[tokio::test]
    async fn test_list_notifications() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("system.listNotifications", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
        // Exactly 6 events — matches original aria2 `rpcNotificationsNames`
        // (BitTorrent enabled is always-on for aria2-rust).
        assert_eq!(
            notifications.len(),
            6,
            "Must match original aria2: 6 notifications (no onBtDownloadError)"
        );
        assert!(notifications.contains(&"aria2.onDownloadStart".to_string()));
        assert!(notifications.contains(&"aria2.onDownloadComplete".to_string()));
        assert!(notifications.contains(&"aria2.onBtDownloadComplete".to_string()));
        // Verify the non-standard event is NOT advertised.
        assert!(
            !notifications.contains(&"aria2.onBtDownloadError".to_string()),
            "aria2.onBtDownloadError is a non-standard extension and must NOT be in listNotifications"
        );
    }
}
