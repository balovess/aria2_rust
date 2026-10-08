use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::super::download_engine::DownloadEngine;
use super::*;
use crate::error::Aria2Error;
use crate::request::request_group::DownloadStatus;
use crate::request::request_group_man::PositionMode;
use crate::util::rwlock_ext::RwLockRecover;

fn manager(
    group_man: Arc<RequestGroupMan>,
    command_sender: EngineCommandSender,
) -> DownloadManager {
    DownloadManager::with_event_hooks(
        group_man,
        command_sender,
        Arc::new(DownloadEventHooks::new()),
    )
}

include!("tests/waiting.rs");
include!("tests/lifecycle.rs");
include!("tests/manager_queries.rs");
include!("tests/submissions.rs");
