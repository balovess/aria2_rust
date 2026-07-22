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
//!     };
//!
//!     let manager = HttpConnectionManager::new(&config);
//!     // Use the connection manager...
//! }
//! ```

mod active_connection;
mod manager;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types to preserve the public API
pub use active_connection::ActiveConnection;
pub use manager::HttpConnectionManager;
pub use types::{ConnectionState, HttpConfig, HttpResponse};
