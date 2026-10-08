//! # aria2-rpc
//!
//! RPC (Remote Procedure Call) library for aria2-rust, providing JSON-RPC 2.0,
//! XML-RPC, and WebSocket interfaces compatible with the original aria2 RPC API.
//!
//! ## Modules
//!
//! - **[`json_rpc`]** — JSON-RPC 2.0 protocol implementation: request/response/error
//!   models, batch support, standard error codes (-32700 to -32603), parameter extractors.
//!
//! - **[`xml_rpc`]** — XML-RPC protocol: methodCall/methodResponse/fault encoding,
//!   8 value types (Int/String/Boolean/Double/Array/Struct/Base64/Nil), quick-xml codec.
//!
//! - **[`websocket`]** — Real-time event notifications via WebSocket:
//!   5 core event types plus the BitTorrent completion event when enabled,
//!   `EventPublisher` pub/sub using tokio::broadcast.
//!
//! - **[`server`]** — HTTP server framework: `AuthConfig` (Token + Basic dual auth),
//!   `CorsConfig`, status models (`StatusInfo`, `GlobalStat`, `DownloadStatus`),
//!   GID generation utility.
//!
//! - **[`engine`]** — `RpcEngine` bridge implementing the feature-specific
//!   aria2 RPC catalog (35 core methods, plus BitTorrent/Metalink extensions):
//!   addUri/addTorrent/remove/pause/unpause/tellStatus/tellActive/tellWaiting/
//!   tellStopped/getGlobalStat/getUris/getFiles/getServers/getPeers/
//!   purgeDownloadResult/getGlobalOption/changeGlobalOption/
//!   getOption/changeOption/getVersion/getSessionInfo/saveSession/shutdown/forceShutdown/
//!   system.multicall/system.listMethods/system.listNotifications.
//!
//! `RpcEngine` owns RPC parsing and dispatch, not download state. The
//! `RpcEngine::new()` constructor deliberately uses an `UnsupportedBackend`,
//! so stateful methods such as `aria2.addUri` return an unsupported-operation
//! error. Applications that serve downloads must provide an
//! [`RpcBackend`](backend::RpcBackend) with [`RpcEngine::with_backend`]. The
//! example below uses the backend-independent method catalog.
//!
//! ## Quick Start
//!
//! ```rust
//! use aria2_rpc::engine::RpcEngine;
//! use aria2_rpc::json_rpc::JsonRpcRequest;
//! use serde_json::json;
//!
//! #[tokio::main]
//! async fn main() {
//!     let engine = RpcEngine::new();
//!
//!     let req = JsonRpcRequest::new("system.listMethods", json!([])).with_id("req-1");
//!
//!     let resp = engine.handle_request(&req).await;
//!     let methods = resp.result.expect("method catalog is backend-independent");
//!     assert!(methods.as_array().is_some_and(|methods| {
//!         methods.iter().any(|method| method == "aria2.addUri")
//!     }));
//! }
//! ```
//!
//! ## Compatibility
//!
//! The implemented catalog follows the original aria2 RPC specification at
//! <https://aria2.github.io/manual/en/html/aria2c.html#rpc-interface>.

pub mod backend;
pub mod constants;
pub mod engine;
mod handlers;
pub mod json_rpc;
pub mod rpc_helpers;
pub mod server;
pub mod types;
pub mod websocket;
mod wire;
pub mod xml_rpc;

pub use backend::{
    BackendError, BackendEvent, BackendMetadata, BackendReadSnapshot, BackendRequest,
    BackendResponse, BackendResult, PositionMode, RpcBackend,
};
pub use engine::RpcEngine;
pub use json_rpc::{JSONRPC_VERSION, JsonRpcError, JsonRpcRequest, JsonRpcResponse, parse_request};
pub use server::{
    AuthConfig, CorsConfig, RpcAuthMiddleware, RpcServer, ServerConfig, TlsConfig, TlsError,
};
pub use types::{
    BittorrentInfo, BittorrentMetaInfo, DhtStatus, DownloadStatus, FileInfo, GlobalStat,
    PeerDetails, PeerFlags, PeerInfo, PeerStats, ServerInfo, ServerInfoIndex, SessionInfo,
    StatusInfo, TrackerInfo, UriEntry, UriStatus, VersionInfo, create_gid,
};
pub use websocket::{
    DownloadEvent, EventPublisher, EventType, NotificationBatcher, WsConfig, WsSession,
};
pub use xml_rpc::{XmlRpcMember, XmlRpcRequest, XmlRpcResponse, XmlRpcValue};
