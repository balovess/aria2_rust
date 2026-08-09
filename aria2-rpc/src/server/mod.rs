//! RPC server module: HTTP/HTTPS server, authentication, CORS, TLS, and
//! WebSocket support.
//!
//! The original single-file `server.rs` mixed several unrelated concerns
//! (auth, CORS, TLS, server config, the axum HTTP server, and WebSocket
//! session handling). It has been split into focused submodules:
//!
//! - [`auth`] — `AuthConfig` (token + basic auth) and `RpcAuthMiddleware`.
//! - [`cors`] — `CorsConfig`.
//! - [`tls`] — `TlsConfig` and `TlsError`.
//! - [`config`] — `ServerConfig` aggregation.
//! - [`http_routes`] — `RpcServer` and the axum HTTP route handlers.
//! - [`ws_session`] — WebSocket upgrade, inbound JSON-RPC dispatch, and
//!   outbound event forwarding.
//! - [`test_cert`] — `generate_test_cert()` test utility.

mod auth;
mod config;
mod cors;
mod http_routes;
mod test_cert;
mod tls;
mod ws_session;

// Re-export data model types from types module
pub use super::types::{
    DownloadStatus, FileInfo, GlobalOptions, GlobalStat, PeerInfo, ServerInfo, ServerInfoIndex,
    SessionInfo, StatusInfo, TaskOptions, UriEntry, UriInfo, UriStatus, VersionInfo, create_gid,
};

pub use auth::*;
pub use config::*;
pub use cors::*;
pub use http_routes::*;
#[cfg(test)]
pub use test_cert::*;
pub use tls::*;
pub use ws_session::*;
