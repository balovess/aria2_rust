use std::sync::Arc;
use std::time::Duration;

use crate::engine::command::{Command, ProgressMessage, ProgressUpdate};
use crate::engine::http::download_command::DownloadCommand;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError};
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::{DownloadOptions, FollowMode, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0; 1024];
    loop {
        let bytes = stream
            .read(&mut chunk)
            .await
            .expect("read HTTP fixture request");
        assert_ne!(
            bytes, 0,
            "HTTP fixture closed before request headers arrived"
        );
        request.extend_from_slice(&chunk[..bytes]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

impl DownloadCommand {
    fn has_progress_sender(&self) -> bool {
        self.progress_sender.is_some()
    }

    fn has_progress_receiver(&self) -> bool {
        self.progress_receiver.is_some()
    }

    fn has_progress_aggregator_handle(&self) -> bool {
        self.progress_aggregator_handle.is_some()
    }

    fn send_progress_update(&self, update: ProgressUpdate) {
        if let Some(ref sender) = self.progress_sender {
            sender
                .try_send(ProgressMessage::Update(update))
                .expect("progress test channel should accept the update");
        } else {
            panic!("test called send_progress_update but no sender is set");
        }
    }
}

mod command_policy;

mod filename_metadata;

mod network_auth;

mod resume;
