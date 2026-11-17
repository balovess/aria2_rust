//! Tests for session persistence module.

use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::resume_data::ResumeData;
use crate::request::request_group::{
    DownloadOptions, FollowMode, GroupId, MetadataInfo, RequestGroup,
};
use crate::util::rwlock_ext::RwLockRecover;

mod sftp_options;

/// Helper to create a temporary directory for tests
fn create_test_session_dir() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        % 1_000_000_000;
    let dir =
        std::env::temp_dir().join(format!("aria2_session_test_{}_{}", std::process::id(), ts));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("Failed to create test session directory");
    dir
}

/// Helper to create test RequestGroups
fn create_test_groups(count: usize) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
    let mut groups = Vec::new();
    for i in 0..count {
        let gid = GroupId::new(i as u64 + 1000);
        let uri = format!("http://example.com/file{}.bin", i);
        let options = DownloadOptions {
            dir: Some("/downloads".to_string()),
            split: Some(4),
            ..Default::default()
        };
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri],
            options,
        )));
        groups.push(group);
    }
    groups
}

mod core;
mod extensions;
mod graphs;
