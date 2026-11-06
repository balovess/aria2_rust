//! End-to-end coverage for durable in-flight multi-mirror HTTP resume.

mod e2e_helpers;

use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::filesystem::control_file::{ControlFile, ControlFileInFlightPiece};
use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::e2e_helpers::mock_http_server::{MockHttpServer, Response, StatusCode, empty_body};

fn generate_test_data(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

fn make_url(base: &str, path: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{}{}", trimmed, path)
    } else {
        format!("{}/{}", trimmed, path)
    }
}

fn make_options(
    split: Option<u16>,
    max_conn: Option<u16>,
    dir: &str,
    out: &str,
) -> DownloadOptions {
    DownloadOptions {
        split,
        max_connection_per_server: max_conn,
        use_head: true,
        max_download_limit: None,
        max_upload_limit: None,
        dir: Some(dir.to_string()),
        out: Some(out.to_string()),
        ..Default::default()
    }
}

async fn wait_for_group_status(
    group: &Arc<std::sync::RwLock<RequestGroup>>,
    expected: DownloadStatus,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().status() == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download did not reach the expected lifecycle state");
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
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("download did not reach the expected completed length");
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

async fn wait_for_engine(
    handle: tokio::task::JoinHandle<aria2_core::error::Result<()>>,
    message: &str,
) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), handle)
        .await
        .expect(message)
        .expect("download engine task panicked");
    result.expect("download engine returned an error");
}

fn has_range_header(entry: &crate::e2e_helpers::mock_http_server::RequestLog) -> bool {
    entry
        .headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("range"))
}
#[tokio::test]
async fn test_multi_mirror_pause_persists_durable_inflight_blocks_and_reassigns_them() {
    const BLOCK_SIZE: usize = 16 * 1024;
    let primary = MockHttpServer::start().await.expect("start primary mirror");
    let replacement = MockHttpServer::start()
        .await
        .expect("start replacement mirror");
    let file_size = 16 * 1024 * 1024;
    let data = Arc::new(generate_test_data(file_size, 113));
    let reject_primary_ranges = Arc::new(AtomicBool::new(false));
    let primary_body = Arc::clone(&data);
    let reject_flag = Arc::clone(&reject_primary_ranges);
    primary.on("GET", "/inflight-resume", move |request| {
        if request.method() == hyper::Method::HEAD {
            return Response::builder()
                .status(StatusCode::OK)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", primary_body.len())
                .body(empty_body())
                .unwrap();
        }
        let Some((start, end)) = request
            .headers()
            .get("Range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, end)| {
                Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
            })
        else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(empty_body())
                .unwrap();
        };
        if reject_flag.load(Ordering::SeqCst) {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(empty_body())
                .unwrap();
        }
        if start > end || end >= primary_body.len() {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{}", primary_body.len()))
                .body(empty_body())
                .unwrap();
        }
        let range = Arc::new(primary_body[start..=end].to_vec());
        let response_body = futures::stream::unfold(0usize, move |offset| {
            let range = Arc::clone(&range);
            async move {
                if offset >= range.len() {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let next = (offset + BLOCK_SIZE).min(range.len());
                Some((
                    Ok::<_, std::convert::Infallible>(Frame::data(Bytes::copy_from_slice(
                        &range[offset..next],
                    ))),
                    next,
                ))
            }
        });
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Accept-Ranges", "bytes")
            .header(
                "Content-Range",
                format!("bytes {start}-{end}/{}", primary_body.len()),
            )
            .header("Content-Length", end - start + 1)
            .body(StreamBody::new(response_body).boxed())
            .unwrap()
    });
    replacement.register_range_response("/inflight-resume", &data);

    let dir = tempfile::tempdir().expect("create output directory");
    let output_name = "multi-mirror-inflight-resume.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let primary_url = make_url(&primary.base_url(), "/inflight-resume");
    let replacement_url = make_url(&replacement.base_url(), "/inflight-resume");
    let mut options = make_options(Some(4), Some(4), &dir.path().to_string_lossy(), output_name);
    options.continue_download = true;
    options.allow_overwrite = true;
    let gid = GroupId::new(478);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![primary_url, replacement_url],
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
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("add multi-mirror download");
    let engine_task = tokio::spawn(engine.run());

    wait_for_group_status(&group, DownloadStatus::Active).await;
    wait_for_completed_length_at_least(&group, 512 * 1024).await;
    command_tx
        .send(EngineCommand::Pause { gid })
        .expect("pause multi-mirror download");
    wait_for_group_status(&group, DownloadStatus::Paused).await;
    let checkpoint = wait_for_in_flight_checkpoint(&control_path).await;
    assert_eq!(checkpoint.piece_length(), Some(1024 * 1024));
    assert!(checkpoint.completed_length() <= file_size as u64);
    assert!(
        !checkpoint.in_flight_pieces().is_empty(),
        "pause must persist partially received multi-mirror pieces"
    );
    let output = tokio::fs::read(&output_path)
        .await
        .expect("read paused output after the writer sync barrier");
    for piece_index in 0..file_size / (1024 * 1024) {
        let byte_index = piece_index / 8;
        let bit_index = piece_index % 8;
        let is_completed = checkpoint
            .bitfield()
            .get(byte_index)
            .is_some_and(|byte| byte & (1 << (7 - bit_index)) != 0);
        if is_completed {
            let start = piece_index * 1024 * 1024;
            let end = (start + 1024 * 1024).min(file_size);
            assert_eq!(
                &output[start..end],
                &data[start..end],
                "checkpoint marks corrupted or unsynced bytes complete for piece {piece_index}"
            );
        }
    }
    let mut expected_resume_offsets = Vec::new();
    let mut persisted_in_flight_bytes = 0usize;
    for piece in checkpoint.in_flight_pieces() {
        let piece_offset = piece.index as usize * 1024 * 1024;
        let block_count = (piece.length as usize).div_ceil(BLOCK_SIZE);
        let mut first_gap = false;
        let mut downloaded_blocks = 0usize;
        for block_index in 0..block_count {
            let is_set = piece.bitfield[block_index / 8] & (1 << (7 - block_index % 8)) != 0;
            if !is_set {
                first_gap = true;
                continue;
            }
            assert!(
                !first_gap,
                "in-flight bitmap must describe a contiguous prefix"
            );
            let start = piece_offset + block_index * BLOCK_SIZE;
            let end = (start + BLOCK_SIZE).min(piece_offset + piece.length as usize);
            assert_eq!(
                &output[start..end],
                &data[start..end],
                "checkpoint claims bytes that were not durably written for piece {} block {}",
                piece.index,
                block_index
            );
            downloaded_blocks += 1;
        }
        assert!(
            downloaded_blocks > 0,
            "persisted in-flight entry has no reusable block"
        );
        let reusable_bytes = (downloaded_blocks * BLOCK_SIZE).min(piece.length as usize);
        persisted_in_flight_bytes += reusable_bytes;
        expected_resume_offsets.push((piece.index, piece_offset + reusable_bytes));
    }
    assert!(
        persisted_in_flight_bytes >= 512 * 1024,
        "checkpoint preserved only {persisted_in_flight_bytes} bytes although pause began after at least 512 KiB of live progress"
    );

    let initial_primary_ranges = primary
        .take_request_log()
        .into_iter()
        .filter_map(|entry| {
            entry
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                .map(|(_, value)| value)
        })
        .filter(|range| range != "bytes=0-0")
        .collect::<Vec<_>>();
    let initial_replacement_ranges = replacement
        .take_request_log()
        .into_iter()
        .filter(has_range_header)
        .collect::<Vec<_>>();
    assert!(
        !initial_primary_ranges.is_empty(),
        "primary mirror must serve initial ranges"
    );
    assert!(
        initial_replacement_ranges.is_empty(),
        "replacement should not serve before reassignment"
    );

    reject_primary_ranges.store(true, Ordering::SeqCst);
    command_tx
        .send(EngineCommand::Unpause { gid })
        .expect("resume multi-mirror download");
    wait_for_engine(engine_task, "multi-mirror resume did not finish").await;

    let reassigned_ranges = replacement
        .take_request_log()
        .into_iter()
        .filter_map(|entry| {
            entry
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                .map(|(_, value)| value)
        })
        .filter(|range| range != "bytes=0-0")
        .collect::<Vec<_>>();
    for (piece_index, expected_offset) in expected_resume_offsets {
        assert!(
            reassigned_ranges.iter().any(|range| {
                range
                    .strip_prefix("bytes=")
                    .and_then(|value| value.split_once('-'))
                    .and_then(|(start, _)| start.parse::<usize>().ok())
                    == Some(expected_offset)
            }),
            "piece {piece_index} was not reassigned from its first unpersisted byte {expected_offset}: {reassigned_ranges:?}"
        );
    }
    assert_eq!(tokio::fs::read(&output_path).await.unwrap(), *data);
    assert!(
        !control_path.exists(),
        "successful completion removes its checkpoint"
    );

    primary.shutdown().await;
    replacement.shutdown().await;
}

#[tokio::test]
async fn test_multi_mirror_resume_rejects_sparse_and_incompatible_inflight_records() {
    const BLOCK_SIZE: usize = 16 * 1024;
    const PIECE_SIZE: usize = 1024 * 1024;
    let primary = MockHttpServer::start().await.expect("start primary mirror");
    let replacement = MockHttpServer::start()
        .await
        .expect("start replacement mirror");
    let file_size = 3 * PIECE_SIZE;
    let data = generate_test_data(file_size, 157);
    primary.register_range_response("/invalid-inflight", &data);
    replacement.register_range_response("/invalid-inflight", &data);

    let dir = tempfile::tempdir().expect("create output directory");
    let output_name = "multi-mirror-invalid-inflight.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let mut seeded_output = vec![0xE7; file_size];
    seeded_output[..BLOCK_SIZE].copy_from_slice(&data[..BLOCK_SIZE]);
    seeded_output[PIECE_SIZE..2 * PIECE_SIZE].copy_from_slice(&data[PIECE_SIZE..2 * PIECE_SIZE]);
    tokio::fs::write(&output_path, seeded_output)
        .await
        .expect("seed a durable block and an out-of-order completed piece");
    let mut checkpoint = ControlFile::open_or_create_with_piece_length(
        &control_path,
        file_size as u64,
        PIECE_SIZE as u32,
    )
    .await
    .expect("create compatible checkpoint geometry");
    checkpoint.mark_piece_done(1);
    checkpoint.update_completed_length((PIECE_SIZE + BLOCK_SIZE) as u64);
    checkpoint.set_in_flight_pieces(vec![
        ControlFileInFlightPiece {
            index: 0,
            length: PIECE_SIZE as u32,
            // Blocks 0 and 2 are marked, but block 1 is a hole. Only block 0
            // forms a safe prefix; block 2 must be fetched again.
            bitfield: vec![0b1010_0000, 0, 0, 0, 0, 0, 0, 0],
        },
        ControlFileInFlightPiece {
            index: 2,
            // This record is structurally valid in the sidecar but does not match
            // the actual 1 MiB final-piece geometry and must be discarded.
            length: (PIECE_SIZE / 2) as u32,
            bitfield: vec![0b1000_0000, 0, 0, 0],
        },
    ]);
    checkpoint.save().await.expect("persist crafted checkpoint");

    let primary_url = make_url(&primary.base_url(), "/invalid-inflight");
    let replacement_url = make_url(&replacement.base_url(), "/invalid-inflight");
    let mut options = make_options(Some(4), Some(4), &dir.path().to_string_lossy(), output_name);
    options.continue_download = true;
    options.allow_overwrite = true;
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(479),
        vec![primary_url, replacement_url],
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
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("add resumed multi-mirror download");
    let engine_task = tokio::spawn(engine.run());
    wait_for_engine(engine_task, "multi-mirror resume did not finish").await;

    let requested_ranges = primary
        .take_request_log()
        .into_iter()
        .chain(replacement.take_request_log())
        .filter_map(|entry| {
            entry
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                .map(|(_, value)| value)
        })
        .filter(|range| range != "bytes=0-0")
        .collect::<Vec<_>>();
    assert!(
        requested_ranges.iter().any(|range| {
            range
                .strip_prefix("bytes=")
                .and_then(|value| value.split_once('-'))
                .and_then(|(start, _)| start.parse::<usize>().ok())
                == Some(BLOCK_SIZE)
        }),
        "resume must begin at the first missing block, not trust a later set bit: {requested_ranges:?}"
    );
    assert!(
        requested_ranges.iter().any(|range| {
            range
                .strip_prefix("bytes=")
                .and_then(|value| value.split_once('-'))
                .and_then(|(start, _)| start.parse::<usize>().ok())
                == Some(2 * PIECE_SIZE)
        }),
        "resume must refetch the full piece when its saved geometry is incompatible: {requested_ranges:?}"
    );
    assert!(
        requested_ranges.iter().all(|range| {
            range
                .strip_prefix("bytes=")
                .and_then(|value| value.split_once('-'))
                .and_then(|(start, _)| start.parse::<usize>().ok())
                .is_none_or(|start| !(PIECE_SIZE..2 * PIECE_SIZE).contains(&start))
        }),
        "aggregate completed_length must not cause a contiguous-prefix resume: {requested_ranges:?}"
    );
    assert_eq!(
        tokio::fs::read(&output_path).await.unwrap(),
        data,
        "a discontiguous checkpoint must not preserve stale bytes after its first gap"
    );
    assert!(
        !control_path.exists(),
        "successful completion removes its checkpoint"
    );

    primary.shutdown().await;
    replacement.shutdown().await;
}
