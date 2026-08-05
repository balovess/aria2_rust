//! HTTP connection manager
//!
//! Provides connection pool reuse, Keep-Alive management, LRU eviction strategy, and redirect following.
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2_core::http::connection::{HttpConnectionManager, HttpConfig};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     let config = HttpConfig {
//!         max_connections: 10,
//!         connect_timeout: Duration::from_secs(30),
//!         read_timeout: Duration::from_secs(60),
//!         write_timeout: Duration::from_secs(60),
//!         idle_timeout: Duration::from_secs(300),
//!         max_idle_per_host: 8,
//!     };
//!
//!     let manager = HttpConnectionManager::with_shared_cookie_storage(&config);
//!     // Use the connection manager with process-wide CookieStorage...
//! }
//! ```

mod active_connection;
mod context;
pub mod happy_eyeballs;
mod manager;
mod pipeline;
mod types;
pub mod write_buffer;

#[cfg(test)]
mod tests;

// Re-export all public types to preserve the public API
pub use active_connection::{ActiveConnection, ConnectionPoolKey, ProxyInfo};
pub use context::ConnectionContext;
pub use happy_eyeballs::{HappyEyeballsResult, connect_with_happy_eyeballs, resolve_dual_stack};
pub use manager::HttpConnectionManager;
pub use pipeline::{HttpPipelineConnection, NtlmState, PendingRequest, PipelineResponse};
pub use types::{ConnectionState, HttpConfig, HttpResponse};
