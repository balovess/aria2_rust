use super::discovery::{
    append_unique_tracker_peers, metadata_tracker_urls, tracker_peer_socket_addr,
};
use std::net::SocketAddr;

use super::*;
use crate::engine::bittorrent::download::command_tests::{
    build_private_test_torrent, build_test_torrent,
};

#[derive(Default)]
struct MetadataEventListener {
    events: std::sync::Mutex<Vec<MetadataResolvedEvent>>,
    alive: std::sync::atomic::AtomicBool,
}

impl MetadataEventListener {
    fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
            alive: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl crate::engine::download_event_hooks::DownloadEventListener for MetadataEventListener {
    fn on_download_event(
        &self,
        _event: crate::engine::download_event_hooks::DownloadEvent,
        _gid: &str,
    ) {
    }

    fn on_metadata_resolved(&self, event: &MetadataResolvedEvent) {
        self.events.lock().unwrap().push(event.clone());
    }

    fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Valid 40-char hex info-hash magnet link used by all test cases.
const TEST_MAGNET_URI: &str =
    "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=test_file";

fn make_test_command() -> MagnetDownloadCommand {
    MagnetDownloadCommand::new(
        GroupId::new(1),
        TEST_MAGNET_URI,
        &DownloadOptions::default(),
        None,
    )
    .expect("Failed to create test MagnetDownloadCommand")
}

fn make_test_command_in_dir(dir: &std::path::Path) -> MagnetDownloadCommand {
    MagnetDownloadCommand::new(
        GroupId::new(2),
        TEST_MAGNET_URI,
        &DownloadOptions::default(),
        dir.to_str(),
    )
    .expect("Failed to create MagnetDownloadCommand in temporary directory")
}

mod constructor;
mod discovery;
mod exact_source;
mod metadata;
