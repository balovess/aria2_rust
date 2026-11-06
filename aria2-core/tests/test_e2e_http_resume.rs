mod e2e_helpers;

use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use std::sync::Arc;

use crate::e2e_helpers::mock_http_server::{MockHttpServer, RequestLog};

fn generate_test_data(size: usize) -> Vec<u8> {
    (0..size)
        .map(|index| (index as u8).wrapping_add(53))
        .collect()
}

async fn wait_for_group_status(
    group: &Arc<std::sync::RwLock<RequestGroup>>,
    expected: DownloadStatus,
    phase: &str,
) {
    let reached = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().status() == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();

    if !reached {
        let group = group.read().unwrap();
        panic!(
            "download did not reach expected lifecycle state during {phase}: expected={expected:?}, observed={:?}, completed_length={}",
            group.status(),
            group.get_completed_length(),
        );
    }
}

async fn wait_for_paused_group_requeue(
    group: &Arc<std::sync::RwLock<RequestGroup>>,
    group_man: &RequestGroupMan,
    phase: &str,
) {
    let requeued = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let (status, num_commands) = {
                let group = group.read().unwrap();
                (group.status(), group.num_commands())
            };
            if status == DownloadStatus::Paused
                && num_commands == 0
                && group_man.active_count() == 0
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();

    if !requeued {
        let (status, num_commands) = {
            let group = group.read().unwrap();
            (group.status(), group.num_commands())
        };
        let active_groups = group_man.active_count();
        panic!(
            "paused download was not fully requeued during {phase}: observed={:?}, num_commands={}, active_groups={}",
            status, num_commands, active_groups,
        );
    }
}

async fn wait_for_completed_length_at_least(
    group: &Arc<std::sync::RwLock<RequestGroup>>,
    expected: u64,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().get_completed_length() >= expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download did not reach the expected completed length");
}

fn payload_range_start(entry: &RequestLog) -> Option<u64> {
    entry
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .and_then(|(_, value)| value.strip_prefix("bytes="))
        .filter(|value| *value != "0-0")
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, _)| start.parse().ok())
}

async fn wait_for_engine(handle: tokio::task::JoinHandle<aria2_core::error::Result<()>>) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), handle)
        .await
        .expect("download engine did not finish")
        .expect("download engine task panicked");
    result.expect("download engine returned an error");
}

async fn wait_for_in_flight_checkpoint(path: &std::path::Path) -> ControlFile {
    // Paused status is published before the cancelled command finishes saving.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(Some(checkpoint)) = ControlFile::load(path).await
                && !checkpoint.in_flight_pieces().is_empty()
            {
                return checkpoint;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pause did not persist reusable in-flight blocks")
}

#[tokio::test]
async fn single_source_pause_persists_in_flight_blocks_and_resumes_from_them() {
    const BLOCK_SIZE: usize = 16 * 1024;
    const PIECE_SIZE: usize = 1024 * 1024;
    let server = MockHttpServer::start()
        .await
        .expect("start mock HTTP server");
    let file_size = 16 * PIECE_SIZE;
    let data = generate_test_data(file_size);
    server.register_slow_range_response("/single-inflight", &data, BLOCK_SIZE, 50);

    let directory = tempfile::tempdir().expect("create output directory");
    let output_name = "single-inflight.bin";
    let output_path = directory.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("{}/single-inflight", server.base_url());
    let options = DownloadOptions {
        allow_overwrite: true,
        continue_download: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        max_connection_per_server: Some(4),
        out: Some(output_name.to_string()),
        split: Some(4),
        use_head: true,
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(415);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url],
        options,
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));

    let mut engine = DownloadEngine::new();
    let group_man = Arc::new(RequestGroupMan::new());
    engine.set_request_group_man(Arc::clone(&group_man));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("add download command");
    let engine_task = tokio::spawn(engine.run());

    wait_for_group_status(&group, DownloadStatus::Active, "initial activation").await;
    // Less than one piece per connection: nothing is complete yet.
    wait_for_completed_length_at_least(&group, 512 * 1024).await;
    command_tx
        .send(EngineCommand::Pause { gid })
        .expect("pause command");
    wait_for_paused_group_requeue(&group, &group_man, "pause").await;
    let checkpoint = wait_for_in_flight_checkpoint(&control_path).await;
    assert_eq!(checkpoint.piece_length(), Some(PIECE_SIZE as u32));

    let output = tokio::fs::read(&output_path)
        .await
        .expect("read paused output after the writer sync barrier");
    let mut expected_resume_offsets = Vec::new();
    let mut persisted_in_flight_bytes = 0;
    for piece in checkpoint.in_flight_pieces() {
        let piece_offset = piece.index as usize * PIECE_SIZE;
        let blocks = (0..(piece.length as usize).div_ceil(BLOCK_SIZE))
            .take_while(|block| piece.bitfield[block / 8] & (1 << (7 - block % 8)) != 0)
            .count();
        let reusable = (blocks * BLOCK_SIZE).min(piece.length as usize);
        assert!(
            reusable > 0,
            "persisted in-flight entry has no reusable block"
        );
        assert_eq!(
            &output[piece_offset..piece_offset + reusable],
            &data[piece_offset..piece_offset + reusable],
            "checkpoint claims bytes of piece {} that were not written",
            piece.index
        );
        persisted_in_flight_bytes += reusable;
        expected_resume_offsets.push((piece.index, (piece_offset + reusable) as u64));
    }
    assert!(
        persisted_in_flight_bytes >= 512 * 1024,
        "checkpoint kept only {persisted_in_flight_bytes} in-flight bytes after at least 512 KiB of progress"
    );
    server.take_request_log();

    command_tx
        .send(EngineCommand::Unpause { gid })
        .expect("resume command");
    wait_for_engine(engine_task).await;

    let resumed_starts = server
        .take_request_log()
        .iter()
        .filter_map(payload_range_start)
        .collect::<Vec<_>>();
    for (piece_index, expected_offset) in expected_resume_offsets {
        assert!(
            resumed_starts.contains(&expected_offset),
            "piece {piece_index} did not resume from its first unsaved byte {expected_offset}: {resumed_starts:?}"
        );
    }
    assert_eq!(tokio::fs::read(&output_path).await.unwrap(), data);
    assert!(!control_path.exists());
    server.shutdown().await;
}

#[tokio::test]
async fn sequential_stream_break_checkpoints_every_written_byte() {
    let file_size = 8 * 1024 * 1024;
    let served = 1536 * 1024;
    let data = generate_test_data(file_size);
    let broken = MockHttpServer::start()
        .await
        .expect("start mock HTTP server");
    broken.register_partial_serve("/broken-stream", &data, served);

    let directory = tempfile::tempdir().expect("create output directory");
    let output_name = "broken-stream.bin";
    let output_path = directory.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let options = DownloadOptions {
        allow_overwrite: true,
        continue_download: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        max_retries: 1,
        out: Some(output_name.to_string()),
        split: Some(1),
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(416),
        vec![format!("{}/broken-stream", broken.base_url())],
        options.clone(),
    )));
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("add download command");
    wait_for_engine(tokio::spawn(engine.run())).await;
    assert!(matches!(
        group.read().unwrap().status(),
        DownloadStatus::Error(_)
    ));

    // The periodic checkpoint is at 1 MiB; the stream broke 512 KiB later.
    let checkpoint = ControlFile::load(&control_path)
        .await
        .expect("load checkpoint")
        .expect("a broken stream keeps its checkpoint");
    assert_eq!(checkpoint.completed_length(), served as u64);
    let output = tokio::fs::read(&output_path).await.unwrap();
    assert_eq!(&output[..served], &data[..served]);
    broken.shutdown().await;

    // A new download of the same file carries on from the break.
    let server = MockHttpServer::start()
        .await
        .expect("start mock HTTP server");
    server.register_range_response("/broken-stream", &data);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(417),
        vec![format!("{}/broken-stream", server.base_url())],
        options,
    )));
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("add retry command");
    wait_for_engine(tokio::spawn(engine.run())).await;

    let first_start = server
        .take_request_log()
        .iter()
        .filter_map(payload_range_start)
        .min();
    assert_eq!(first_start, Some(served as u64));
    assert_eq!(tokio::fs::read(&output_path).await.unwrap(), data);
    assert!(!control_path.exists());
    server.shutdown().await;
}
