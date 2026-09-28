//! HTTP task execution: request policy, range probing, and concurrent or
//! sequential transfer paths.

pub(crate) mod adaptive_concurrency;
pub(crate) mod auth;
pub(crate) mod client_config;
pub(crate) mod command_factory;
pub(crate) mod concurrent_download;
pub mod cookie_helper;
pub mod download_command;
pub(crate) mod request_executor;
pub mod segment_downloader;
pub mod sequential_download;
pub(crate) mod tail_reclaim;
