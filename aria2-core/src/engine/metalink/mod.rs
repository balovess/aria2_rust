//! Metalink metadata and request-graph orchestration over the payload protocols.

pub mod download_command;
pub mod post_download_handler;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
pub mod request_graph;
pub mod to_request_group;
