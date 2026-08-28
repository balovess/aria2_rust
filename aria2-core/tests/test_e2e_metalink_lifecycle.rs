#![cfg(feature = "metalink")]

mod e2e_helpers;

#[cfg(feature = "bittorrent")]
mod fixtures;

use aria2_core::engine::command::Command;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
use aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup;
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::request::request_group::{
    DownloadOptions, DownloadStatus, FollowMode, GroupId, RequestGroup,
};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
use bytes::Bytes;
use e2e_helpers::mock_http_server::full_body;
use e2e_helpers::mock_http_server::{Incoming, MockHttpServer, Request, Response, StatusCode};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn metalink(name: &str, url: &str, data: &[u8]) -> Vec<u8> {
    use sha2::Digest;

    let digest = format!("{:x}", sha2::Sha256::digest(data));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="{name}">
    <size>{}</size>
    <hash type="sha-256">{digest}</hash>
    <url priority="1">{url}</url>
  </file>
</metalink>"#,
        data.len()
    )
    .into_bytes()
}

fn range_start(request: &Request<Incoming>) -> Option<usize> {
    request
        .headers()
        .get("Range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, _)| start.parse().ok())
}

fn install_slow_range_route(server: &MockHttpServer, path: &str, data: &[u8]) {
    let path = path.to_string();
    let data = Arc::new(data.to_vec());
    server.on_get(&path, move |request: &Request<Incoming>| {
        let requested_start = range_start(request);
        let start = requested_start.unwrap_or(0);
        let end = data.len().saturating_sub(1);
        let body = Arc::clone(&data);
        let stream = futures::stream::unfold(start, move |offset| {
            let body = Arc::clone(&body);
            async move {
                if offset > end {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(8)).await;
                let next = (offset + 16 * 1024).min(end + 1);
                Some((
                    Ok::<_, Infallible>(Frame::data(Bytes::copy_from_slice(&body[offset..next]))),
                    next,
                ))
            }
        });

        let mut response = Response::builder()
            .status(if requested_start.is_none() {
                StatusCode::OK
            } else {
                StatusCode::PARTIAL_CONTENT
            })
            .header("Accept-Ranges", "bytes")
            .header("Content-Length", data.len().saturating_sub(start));
        if requested_start.is_some() {
            response = response.header(
                "Content-Range",
                format!("bytes={start}-{end}/{}", data.len()),
            );
        }
        response.body(StreamBody::new(stream).boxed()).unwrap()
    });
}

async fn wait_for_partial_output(
    group: &Arc<std::sync::RwLock<aria2_core::request::request_group::RequestGroup>>,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if group.recover().completed_length() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Metalink transfer did not make progress");
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn metalink_engine_promotes_torrent_payload_and_preserves_mapping() {
    let server = MockHttpServer::start().await.unwrap();
    let tracker =
        fixtures::mock_tracker::MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let directory = tempfile::tempdir().unwrap();
    let total_size = 32 * 1024;
    let piece_length = 1024;
    let expected = fixtures::test_torrent_builder::generate_file_data(total_size);
    let web_seed_path = "/payload.bin";
    server.register_slow_range_response(web_seed_path, &expected, 256, 5);

    let web_seed_url = format!("{}{}", server.base_url(), web_seed_path);
    let torrent = fixtures::test_torrent_builder::build_test_torrent_with_web_seeds(
        "payload.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
        std::slice::from_ref(&web_seed_url),
    );
    let torrent_for_server = torrent.clone();
    server.on_get("/payload.torrent", move |_request: &Request<Incoming>| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/x-bittorrent")
            .header("Content-Length", torrent_for_server.len())
            .body(full_body(torrent_for_server.clone()))
            .unwrap()
    });

    let metadata_url = format!("{}/payload.torrent", server.base_url());
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="renamed-payload.bin">
    <size>{total_size}</size>
    <metaurl name="payload.bin" mediatype="torrent">{metadata_url}</metaurl>
  </file>
</metalink>"#
    )
    .into_bytes();
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        enable_dht: false,
        enable_public_trackers: false,
        seed_time: Some(0.0),
        ..DownloadOptions::default()
    };

    let mut gids = [GroupId::new(900), GroupId::new(901)].into_iter();
    let mut graphs = MetalinkToRequestGroup::new()
        .create_torrent_graphs_from_bytes(&document, &options, &mut gids)
        .unwrap();
    assert_eq!(graphs.len(), 1);
    let graph = graphs.pop().unwrap();
    let metadata = Arc::clone(&graph.metadata);
    let payload = Arc::clone(&graph.payload);
    let payload_gid = payload.recover().gid();

    let group_man = Arc::new(RequestGroupMan::new());
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::clone(&group_man));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddMetalinkGraph { graph })
        .unwrap();

    let engine_task = tokio::spawn(engine.run());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if payload.recover().status() == DownloadStatus::Active {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("torrent payload was not promoted by the engine");

    let mapped_path = directory.path().join("renamed-payload.bin");
    let mapped_context = payload
        .recover()
        .get_download_context()
        .expect("promotion should install the parsed torrent context");
    let mapped_entry = mapped_context
        .get_file_entries()
        .first()
        .expect("single-file torrent should have one mapped entry")
        .path()
        .to_owned();
    assert_eq!(mapped_entry, mapped_path.to_string_lossy());

    tokio::time::timeout(Duration::from_secs(30), engine_task)
        .await
        .expect("Metalink engine lifecycle timed out")
        .expect("Metalink engine task panicked")
        .expect("Metalink engine returned an error");

    assert_eq!(tokio::fs::read(&mapped_path).await.unwrap(), expected);
    assert_eq!(metadata.recover().status(), DownloadStatus::Complete);
    assert_eq!(payload_gid, GroupId::new(901));
    assert!(!directory.path().join("payload.torrent").exists());

    let metadata_requests = server
        .take_request_log()
        .into_iter()
        .filter(|request| request.path == "/payload.torrent")
        .count();
    assert_eq!(metadata_requests, 1, "metadata must be downloaded once");
}

#[tokio::test]
async fn metalink_stream_pause_resume_persists_rust_checkpoint() {
    let server = MockHttpServer::start().await.unwrap();
    let data = payload(2 * 1024 * 1024);
    install_slow_range_route(&server, "/metalink-pause.bin", &data);
    let directory = tempfile::tempdir().unwrap();
    let url = format!("{}/metalink-pause.bin", server.base_url());
    let document = metalink("metalink-pause.bin", &url, &data);
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        ..DownloadOptions::default()
    };

    let mut command = MetalinkDownloadCommand::new(
        GroupId::new(700),
        &document,
        &options,
        directory.path().to_str(),
    )
    .unwrap();
    let group = command.request_group().unwrap();
    let task = tokio::spawn(async move { command.execute().await });
    wait_for_partial_output(&group).await;
    group.recover_mut().pause().unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("paused Metalink command timed out")
        .expect("paused Metalink task panicked");
    assert!(
        result.is_err(),
        "pause must stop the active Metalink stream"
    );
    assert!(group.recover().status().is_paused());

    let output = directory.path().join("metalink-pause.bin");
    let control = ControlFile::control_path_for(&output);
    let saved = ControlFile::load(&control)
        .await
        .unwrap()
        .expect("pause should leave the Rust checkpoint");
    assert!(saved.completed_length() > 0);
    assert!(saved.completed_length() < data.len() as u64);

    let mut resumed = MetalinkDownloadCommand::new(
        GroupId::new(701),
        &document,
        &options,
        directory.path().to_str(),
    )
    .unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(tokio::fs::read(&output).await.unwrap(), data);
    assert!(
        !control.exists(),
        "completion must remove the Rust checkpoint"
    );

    let requests = server.take_request_log();
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/metalink-pause.bin"
                && request
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("range"))),
        "resume must issue a Range request from the persisted offset"
    );
}

#[tokio::test]
async fn metalink_stream_remove_preserves_partial_output_and_checkpoint() {
    let server = MockHttpServer::start().await.unwrap();
    let data = payload(2 * 1024 * 1024);
    install_slow_range_route(&server, "/metalink-remove.bin", &data);
    let directory = tempfile::tempdir().unwrap();
    let url = format!("{}/metalink-remove.bin", server.base_url());
    let document = metalink("metalink-remove.bin", &url, &data);
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        ..DownloadOptions::default()
    };

    let mut command = MetalinkDownloadCommand::new(
        GroupId::new(702),
        &document,
        &options,
        directory.path().to_str(),
    )
    .unwrap();
    let group = command.request_group().unwrap();
    let task = tokio::spawn(async move { command.execute().await });
    wait_for_partial_output(&group).await;
    group.recover_mut().remove().unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("removed Metalink command timed out")
        .expect("removed Metalink task panicked");
    assert!(
        result.is_err(),
        "remove must stop the active Metalink stream"
    );
    assert!(group.recover().is_removed());

    let output = directory.path().join("metalink-remove.bin");
    let control = ControlFile::control_path_for(&output);
    assert!(output.metadata().unwrap().len() > 0);
    assert!(
        control.exists(),
        "remove must preserve the Rust checkpoint for explicit recovery"
    );
}

#[tokio::test]
async fn metalink_engine_force_halt_preserves_resume_state() {
    let server = MockHttpServer::start().await.unwrap();
    let data = payload(2 * 1024 * 1024);
    install_slow_range_route(&server, "/metalink-force-halt.bin", &data);
    let directory = tempfile::tempdir().unwrap();
    let url = format!("{}/metalink-force-halt.bin", server.base_url());
    let document = metalink("metalink-force-halt.bin", &url, &data);
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        ..DownloadOptions::default()
    };

    let mut gids = [GroupId::new(703)].into_iter();
    let mut groups = MetalinkToRequestGroup::new()
        .create_resource_groups_from_bytes(&document, &options, &mut gids)
        .unwrap();
    assert_eq!(groups.len(), 1);
    let group = groups.pop().unwrap();
    let group_man = Arc::new(RequestGroupMan::new());
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::clone(&group_man));
    let command_tx = engine.engine_command_sender();
    let group_handle = Arc::clone(&group);
    command_tx
        .send(EngineCommand::AddDownload { group })
        .unwrap();

    let engine_task = tokio::spawn(engine.run());
    wait_for_partial_output(&group_handle).await;
    command_tx
        .send(EngineCommand::ForceHaltAll {
            reason: aria2_core::request::request_group::HaltReason::ShutdownSignal,
        })
        .unwrap();

    tokio::time::timeout(Duration::from_secs(10), engine_task)
        .await
        .expect("Metalink force-halt engine did not stop")
        .expect("Metalink force-halt engine task panicked")
        .expect("Metalink force-halt engine returned an error");

    assert_eq!(
        group_handle.recover().create_download_result().code,
        aria2_core::request::request_group::DownloadResultCode::InProgress
    );
    assert_eq!(
        group_handle.recover().get_halt_reason(),
        aria2_core::request::request_group::HaltReason::ShutdownSignal
    );
    let output = directory.path().join("metalink-force-halt.bin");
    let control = ControlFile::control_path_for(&output);
    let saved = ControlFile::load(&control)
        .await
        .unwrap()
        .expect("force-halt should leave the Rust checkpoint");
    assert!(saved.completed_length() > 0);
    assert!(saved.completed_length() < data.len() as u64);
}

#[tokio::test]
async fn metalink_follow_mem_engine_creates_child_without_source_file() {
    let server = MockHttpServer::start().await.unwrap();
    let data = payload(64 * 1024);
    let payload_url = format!("{}/payload.bin", server.base_url());
    let document = metalink("payload.bin", &payload_url, &data);
    let document_for_route = document.clone();
    server.on_get("/index.meta4", move |_request: &Request<Incoming>| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/metalink4+xml")
            .header("Content-Length", document_for_route.len())
            .body(full_body(document_for_route.clone()))
            .unwrap()
    });
    let payload_for_route = data.clone();
    server.on_get("/payload.bin", move |_request: &Request<Incoming>| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", payload_for_route.len())
            .body(full_body(payload_for_route.clone()))
            .unwrap()
    });

    let directory = tempfile::tempdir().unwrap();
    let source_url = format!("{}/index.meta4", server.base_url());
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        follow_metalink: Some(FollowMode::Memory),
        use_head: false,
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(730);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![source_url],
        options,
    )));
    let group_man = Arc::new(RequestGroupMan::new());
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::clone(&group_man));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .unwrap();

    let engine_task = tokio::spawn(engine.run());
    tokio::time::timeout(Duration::from_secs(30), engine_task)
        .await
        .expect("follow-metalink=mem engine lifecycle timed out")
        .expect("follow-metalink=mem engine task panicked")
        .expect("follow-metalink=mem engine returned an error");

    assert_eq!(group.recover().status(), DownloadStatus::Complete);
    let parent = group_man
        .find_stopped_result(&gid.to_hex_string())
        .expect("completed Metalink source must have a stopped result");
    assert_eq!(parent.status, DownloadStatus::Complete);
    assert_eq!(
        parent.followed_by.len(),
        1,
        "parent result did not record a Metalink child: {parent:?}; stopped_count={}",
        group_man.stopped_count()
    );

    let child = group_man
        .find_stopped_result(&parent.followed_by[0].to_hex_string())
        .expect("followed Metalink payload must have a stopped result");
    assert_eq!(child.status, DownloadStatus::Complete);
    assert_eq!(
        tokio::fs::read(directory.path().join("payload.bin"))
            .await
            .unwrap(),
        data
    );
    assert!(
        !directory.path().join("index.meta4").exists(),
        "follow-metalink=mem must not materialize the source document"
    );
}
