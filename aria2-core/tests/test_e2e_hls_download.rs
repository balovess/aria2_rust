mod e2e_helpers;

use aria2_core::engine::command::Command;
use aria2_core::engine::http::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use e2e_helpers::mock_http_server::{MockHttpServer, full_body};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

fn ffmpeg_program() -> Option<OsString> {
    if let Some(configured) = std::env::var_os("ARIA2C_FFMPEG")
        && !configured.is_empty()
    {
        return Some(configured);
    }
    let output = StdCommand::new("ffmpeg").arg("-version").output().ok()?;
    output.status.success().then(|| OsString::from("ffmpeg"))
}

fn generate_hls_fixture(ffmpeg: &OsString, fixture_dir: &Path) {
    let playlist = fixture_dir.join("stream.m3u8");
    let segment_pattern = fixture_dir.join("segment-%d.ts");
    let status = StdCommand::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("testsrc=size=320x180:rate=15")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("sine=frequency=440:sample_rate=44100")
        .arg("-t")
        .arg("2")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg("1")
        .arg("-hls_list_size")
        .arg("0")
        .arg("-hls_segment_filename")
        .arg(segment_pattern)
        .arg(&playlist)
        .status()
        .expect("failed to run ffmpeg fixture generator");
    assert!(
        status.success(),
        "ffmpeg fixture generation failed: {status}"
    );
}

fn generate_encrypted_ts_fixture(ffmpeg: &OsString, fixture_dir: &Path, key_uri: &str) {
    let key = [0x10_u8; 16];
    std::fs::write(fixture_dir.join("key.bin"), key).unwrap();
    let key_path = fixture_dir
        .join("key.bin")
        .to_string_lossy()
        .replace('\\', "/");
    let key_info_path = fixture_dir.join("key.txt");
    std::fs::write(&key_info_path, format!("{key_uri}\n{key_path}\n")).unwrap();

    let playlist = fixture_dir.join("encrypted.m3u8");
    let status = StdCommand::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("testsrc=size=320x180:rate=15")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("sine=frequency=440:sample_rate=44100")
        .arg("-t")
        .arg("2")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg("1")
        .arg("-hls_list_size")
        .arg("0")
        .arg("-hls_segment_filename")
        .arg(fixture_dir.join("segment-%d.ts"))
        .arg("-hls_key_info_file")
        .arg(key_info_path)
        .arg(&playlist)
        .status()
        .expect("failed to run ffmpeg encrypted fixture generator");
    assert!(
        status.success(),
        "encrypted TS fixture generation failed: {status}"
    );
}

fn generate_fmp4_fixture(ffmpeg: &OsString, fixture_dir: &Path) {
    let playlist = fixture_dir.join("fragmented.m3u8");
    let status = StdCommand::new(ffmpeg)
        .current_dir(fixture_dir)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("testsrc=size=320x180:rate=15")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("sine=frequency=440:sample_rate=44100")
        .arg("-t")
        .arg("2")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg("1")
        .arg("-hls_list_size")
        .arg("0")
        .arg("-hls_segment_type")
        .arg("fmp4")
        .arg("-hls_fmp4_init_filename")
        .arg("init.mp4")
        .arg("-hls_segment_filename")
        .arg(fixture_dir.join("segment-%d.m4s"))
        .arg(&playlist)
        .status()
        .expect("failed to run ffmpeg fMP4 fixture generator");
    assert!(status.success(), "fMP4 fixture generation failed: {status}");
}

fn serve_fixture_files(server: &MockHttpServer, fixture_dir: &Path, playlist_name: &str) {
    for entry in std::fs::read_dir(fixture_dir)
        .unwrap()
        .filter_map(Result::ok)
    {
        let name = entry.file_name().into_string().unwrap();
        if name == "key.txt" || name.ends_with(".bin") {
            continue;
        }
        let bytes = std::fs::read(entry.path()).unwrap();
        let content_type = if name == playlist_name {
            "application/vnd.apple.mpegurl"
        } else {
            "application/octet-stream"
        };
        server.on_get(&format!("/{name}"), move |_| {
            http::Response::builder()
                .header("content-type", content_type)
                .body(full_body(bytes.clone()))
                .unwrap()
        });
    }
    if let Ok(key) = std::fs::read(fixture_dir.join("key.bin")) {
        server.on_get("/key.bin", move |_| {
            http::Response::builder()
                .header("content-type", "application/octet-stream")
                .body(full_body(key.clone()))
                .unwrap()
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_hls_playlist_into_playable_mp4() {
    let Some(ffmpeg) = ffmpeg_program() else {
        eprintln!("skipping HLS E2E test: user-installed ffmpeg was not found");
        return;
    };

    let fixture_dir = tempfile::tempdir().unwrap();
    generate_hls_fixture(&ffmpeg, fixture_dir.path());
    std::fs::copy(
        fixture_dir.path().join("segment-0.ts"),
        fixture_dir.path().join("segment-2.ts"),
    )
    .unwrap();

    let playlist = std::fs::read(fixture_dir.path().join("stream.m3u8")).unwrap();
    let segments: Vec<_> = std::fs::read_dir(fixture_dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "ts")
        })
        .map(|entry| {
            (
                entry.file_name().into_string().unwrap(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    assert!(!segments.is_empty(), "fixture must contain TS segments");

    let server = MockHttpServer::start().await.unwrap();
    server.on_get("/stream.m3u8", move |_| {
        http::Response::builder()
            .header("content-type", "application/vnd.apple.mpegurl")
            .body(full_body(playlist.clone()))
            .unwrap()
    });
    for (name, bytes) in segments {
        let path = format!("/{name}");
        server.on_get(&path, move |_| {
            http::Response::builder()
                .header("content-type", "video/mp2t")
                .body(full_body(bytes.clone()))
                .unwrap()
        });
    }

    let output_dir = tempfile::tempdir().unwrap();
    let url = format!("{}/stream.m3u8", server.base_url());
    let mut command = DownloadCommand::new(
        GroupId::new(9101),
        &url,
        &DownloadOptions::default(),
        output_dir.path().to_str(),
        Some("hls-output.mp4"),
    )
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(45), command.execute())
        .await
        .expect("HLS download timed out")
        .expect("HLS download failed");

    let output: PathBuf = output_dir.path().join("hls-output.mp4");
    let metadata = tokio::fs::metadata(&output).await.unwrap();
    assert!(metadata.len() > 0, "HLS output must not be empty");

    let decode = StdCommand::new(&ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(&output)
        .arg("-f")
        .arg("null")
        .arg("-")
        .status()
        .unwrap();
    assert!(decode.success(), "HLS output must be decodable: {decode}");
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_encrypted_hls_playlist() {
    let Some(ffmpeg) = ffmpeg_program() else {
        eprintln!("skipping encrypted HLS E2E test: user-installed ffmpeg was not found");
        return;
    };

    let server = MockHttpServer::start().await.unwrap();
    let key_uri = format!("{}/key.bin", server.base_url());
    let fixture_dir = tempfile::tempdir().unwrap();
    generate_encrypted_ts_fixture(&ffmpeg, fixture_dir.path(), &key_uri);
    serve_fixture_files(&server, fixture_dir.path(), "encrypted.m3u8");

    let output_dir = tempfile::tempdir().unwrap();
    let url = format!("{}/encrypted.m3u8", server.base_url());
    let mut command = DownloadCommand::new(
        GroupId::new(9102),
        &url,
        &DownloadOptions::default(),
        output_dir.path().to_str(),
        Some("encrypted-output.mp4"),
    )
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(45), command.execute())
        .await
        .expect("encrypted HLS download timed out")
        .expect("encrypted HLS download failed");

    let output = output_dir.path().join("encrypted-output.mp4");
    let metadata = tokio::fs::metadata(&output).await.unwrap();
    assert!(metadata.len() > 0, "encrypted HLS output must not be empty");

    let decode = StdCommand::new(&ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(&output)
        .arg("-f")
        .arg("null")
        .arg("-")
        .status()
        .unwrap();
    assert!(
        decode.success(),
        "encrypted HLS output must be decodable: {decode}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_fmp4_hls_playlist() {
    let Some(ffmpeg) = ffmpeg_program() else {
        eprintln!("skipping fMP4 HLS E2E test: user-installed ffmpeg was not found");
        return;
    };

    let server = MockHttpServer::start().await.unwrap();
    let fixture_dir = tempfile::tempdir().unwrap();
    generate_fmp4_fixture(&ffmpeg, fixture_dir.path());
    serve_fixture_files(&server, fixture_dir.path(), "fragmented.m3u8");
    let init_section = std::fs::read(fixture_dir.path().join("init.mp4")).unwrap();
    server.on_get("/init.mp4", move |_| {
        http::Response::builder()
            .header("content-type", "video/mp4")
            .body(full_body(init_section.clone()))
            .unwrap()
    });

    let output_dir = tempfile::tempdir().unwrap();
    let url = format!("{}/fragmented.m3u8", server.base_url());
    let mut command = DownloadCommand::new(
        GroupId::new(9103),
        &url,
        &DownloadOptions::default(),
        output_dir.path().to_str(),
        Some("fmp4-output.mp4"),
    )
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(45), command.execute())
        .await
        .expect("fMP4 HLS download timed out")
        .expect("fMP4 HLS download failed");

    let output = output_dir.path().join("fmp4-output.mp4");
    let metadata = tokio::fs::metadata(&output).await.unwrap();
    assert!(metadata.len() > 0, "fMP4 HLS output must not be empty");

    let decode = StdCommand::new(&ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(&output)
        .arg("-f")
        .arg("null")
        .arg("-")
        .status()
        .unwrap();
    assert!(
        decode.success(),
        "fMP4 HLS output must be decodable: {decode}"
    );
}
