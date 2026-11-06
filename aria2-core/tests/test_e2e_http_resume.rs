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

async fn wait_for_checkpoint_length_at_least(path: &std::path::Path, expected: u64) {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if let Ok(Some(checkpoint)) = ControlFile::load(path).await
                && checkpoint.completed_length() >= expected
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("control file did not persist the expected completed length");
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

#[tokio::test]
async fn named_http_download_advances_checkpoint_across_repeated_pauses() {
    let server = MockHttpServer::start()
        .await
        .expect("start mock HTTP server");
    let file_size = 32 * 1024 * 1024;
    let data = generate_test_data(file_size);
    server.register_slow_range_response("/named-resume", &data, 128 * 1024, 150);

    let directory = tempfile::tempdir().expect("create output directory");
    let output_name = "named-resume.bin";
    let output_path = directory.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("{}/named-resume", server.base_url());
    let options = DownloadOptions {
        allow_overwrite: true,
        continue_download: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        max_connection_per_server: Some(4),
        out: Some(output_name.to_string()),
        split: Some(4),
        use_head: false,
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(414);
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
    wait_for_completed_length_at_least(&group, 2 * 1024 * 1024).await;
    command_tx
        .send(EngineCommand::Pause { gid })
        .expect("initial pause command");
    wait_for_paused_group_requeue(&group, &group_man, "initial pause").await;
    wait_for_checkpoint_length_at_least(&control_path, 2 * 1024 * 1024).await;

    let initial_checkpoint = ControlFile::load(&control_path)
        .await
        .expect("load initial checkpoint")
        .expect("initial pause should create a checkpoint");
    assert_eq!(initial_checkpoint.piece_length(), Some(file_size as u32));
    let mut previous_checkpoint = initial_checkpoint.completed_length();
    assert!(previous_checkpoint >= 2 * 1024 * 1024);
    server.take_request_log();

    let fixed_piece_size = 1024 * 1024;
    let in_flight_allowance = 4 * fixed_piece_size;
    for resume_index in 0..3 {
        command_tx
            .send(EngineCommand::Unpause { gid })
            .expect("resume command");
        let resume_phase = format!("resume {resume_index} activation");
        wait_for_group_status(&group, DownloadStatus::Active, &resume_phase).await;
        wait_for_checkpoint_length_at_least(&control_path, previous_checkpoint + fixed_piece_size)
            .await;
        command_tx
            .send(EngineCommand::Pause { gid })
            .expect("pause command");
        let pause_phase = format!("resume {resume_index} pause");
        wait_for_paused_group_requeue(&group, &group_man, &pause_phase).await;

        let checkpoint = ControlFile::load(&control_path)
            .await
            .expect("load resumed checkpoint")
            .expect("paused resume should preserve its checkpoint");
        assert!(
            checkpoint.completed_length() >= previous_checkpoint + fixed_piece_size,
            "resume {resume_index} did not advance: previous={previous_checkpoint}, current={}",
            checkpoint.completed_length()
        );
        assert_eq!(checkpoint.piece_length(), Some(fixed_piece_size as u32));
        assert!(
            checkpoint.completed_pieces() > 0,
            "resume {resume_index} did not persist restored segments in the bitfield"
        );

        let requests = server.take_request_log();
        let earliest_start = requests
            .iter()
            .filter_map(payload_range_start)
            .min()
            .expect("resumed HTTP requests should carry Range headers");
        let minimum_allowed_start = previous_checkpoint.saturating_sub(in_flight_allowance);
        assert!(
            earliest_start >= minimum_allowed_start,
            "resume {resume_index} restarted too far back: earliest Range={earliest_start}, pause checkpoint={previous_checkpoint}, allowance={in_flight_allowance}, requests={requests:?}, bitfield={:?}, completed_pieces={}, piece_length={:?}",
            checkpoint.bitfield(),
            checkpoint.completed_pieces(),
            checkpoint.piece_length()
        );
        previous_checkpoint = checkpoint.completed_length();
    }

    command_tx
        .send(EngineCommand::Unpause { gid })
        .expect("final resume command");
    wait_for_engine(engine_task).await;

    assert_eq!(tokio::fs::read(&output_path).await.unwrap(), data);
    assert!(!control_path.exists());
    server.shutdown().await;
}
