//! Download task lifecycle, dispatch, and protocol-specific command assembly.
//!
//! Shared scheduling and engine services stay at this level. HTTP, FTP, SFTP,
//! BitTorrent, and Metalink execution live in their matching protocol modules.

pub mod active_output_registry;
pub mod bittorrent;
pub mod command;
pub mod concurrent_segment_manager;
pub mod download_engine;
pub mod download_event_hooks;
pub mod download_manager;
pub(crate) mod download_progress;
pub mod engine_command;
pub mod engine_loop;
pub mod ftp;
pub mod halt_watchers;
#[cfg(feature = "bittorrent")]
pub mod hook_manager;
pub mod http;
#[cfg(feature = "metalink")]
pub mod metalink;
pub mod mirror_coordinator;
pub mod post_download_handler;
mod process_wait;
pub(crate) mod progress_checkpoint;
pub mod resume_data;
pub mod retry_policy;
#[cfg(feature = "sftp")]
pub mod sftp;
pub mod task_spawner;
pub mod timer;
