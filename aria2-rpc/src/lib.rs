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
//!   7 event types (DownloadStart/Pause/Stop/Complete/Error/BtComplete/BtError),
//!   `EventPublisher` pub/sub using tokio::broadcast.
//!
//! - **[`server`]** — HTTP server framework: `AuthConfig` (Token + Basic dual auth),
//!   `CorsConfig`, status models (`StatusInfo`, `GlobalStat`, `DownloadStatus`),
//!   GID generation utility.
//!
//! - **[`engine`]** — `RpcEngine` bridge implementing 25 aria2 RPC methods:
//!   addUri/addTorrent/remove/pause/unpause/tellStatus/tellActive/tellWaiting/
//!   tellStopped/getGlobalStat/purgeDownloadResult/getGlobalOption/changeGlobalOption/
//!   getOption/changeOption/getVersion/getSessionInfo/saveSession/shutdown.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use aria2_rpc::engine::RpcEngine;
//! use aria2_rpc::json_rpc::JsonRpcRequest;
//!
//! #[tokio::main]
//! async fn main() {
//!     let engine = RpcEngine::new();
//!
//!     let req = JsonRpcRequest {
//!         version: Some("2.0".into()),
//!         method: "aria2.addUri".into(),
//!         params: serde_json::json!([["http://example.com/file.zip"]]),
//!         id: Some(serde_json::Value::String("req-1".into())),
//!     };
//!
//!     let resp = engine.handle_request(&req).await;
//!     if resp.is_success() {
//!         println!("GID: {:?}", resp.result);
//!     }
//! }
//! ```
//!
//! ## Compatibility
//!
//! All 25 implemented methods follow the original aria2 RPC specification at
//! <https://aria2.github.io/manual/en/html/aria2c.html#rpc-interface>.

pub mod engine;
pub mod json_rpc;
pub mod server;
pub mod websocket;
pub mod xml_rpc;

pub use engine::RpcEngine;
pub use json_rpc::{parse_request, JsonRpcError, JsonRpcRequest, JsonRpcResponse, JSONRPC_VERSION};
pub use server::{
    create_gid, AuthConfig, CorsConfig, DownloadStatus, FileInfo, GlobalStat, ServerConfig,
    StatusInfo, UriEntry,
};
pub use websocket::{DownloadEvent, EventPublisher, EventType, WsSession};
pub use xml_rpc::{XmlRpcMember, XmlRpcRequest, XmlRpcResponse, XmlRpcValue};
