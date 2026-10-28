// Progress that sequential and gap downloads leave in the control file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::SequentialDownloader;
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::http::cookie_helper::CookieHelper;
use crate::engine::retry_policy::RetryPolicy;
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::resume_helper::ResumeHelper;
use crate::http::HttpRequestPolicy;
use crate::http::cookie::CookieStorage;
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::util::perf_monitor::AtomicMetrics;
use crate::util::rwlock_ext::RwLockRecover;

const KIB: usize = 1024;

fn payload(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

fn make_downloader(uri: &str, output_path: PathBuf, group_id: u64) -> SequentialDownloader {
    crate::http::client_pool::ensure_rustls_provider();
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(group_id),
        vec![uri.to_string()],
        DownloadOptions::default(),
    )));
    group.recover_mut().start().expect("start request group");
    let progress = Arc::new(AtomicProgress::new());
    SequentialDownloader::new(
        Arc::new(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build test HTTP client"),
        ),
        output_path,
        HttpRequestPolicy::default(),
        CookieHelper::new(Arc::new(CookieStorage::new()), None),
        ProgressUpdater::new(
            None,
            None,
            Arc::clone(&progress),
            Arc::new(AtomicMetrics::new()),
            None,
        ),
        group,
        progress,
        None,
    )
}

/// A response with `body`, closed after its first `sent` bytes.
fn response(head: &str, body: &[u8], sent: usize) -> Vec<u8> {
    let mut bytes = format!(
        "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(&body[..sent]);
    bytes
}

/// Serve one scripted response per connection; returns the request heads.
async fn serve(responses: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test HTTP listener");
    let uri = format!(
        "http://{}/payload.bin",
        listener.local_addr().expect("read listener address")
    );
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let accepted =
                tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept()).await;
            let Ok(accepted) = accepted else {
                break;
            };
            let (mut stream, _) = accepted.expect("accept HTTP request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).await.expect("read HTTP request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            requests.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        }
        requests
    });
    (uri, server)
}

/// A split checkpoint over `data` with `done` pieces finished, and an output
/// file holding only those pieces.
async fn split_checkpoint(output: &Path, data: &[u8], piece: usize, done: &[usize]) {
    let mut file = vec![0_u8; data.len()];
    let mut control_file = ControlFile::open_or_create_with_piece_length(
        &ControlFile::control_path_for(output),
        data.len() as u64,
        piece as u32,
    )
    .await
    .expect("create split checkpoint");
    for &index in done {
        let span = index * piece..((index + 1) * piece).min(data.len());
        file[span.clone()].copy_from_slice(&data[span]);
        control_file.mark_piece_done(index);
    }
    control_file.save().await.expect("save split checkpoint");
    tokio::fs::write(output, file)
        .await
        .expect("write partial output");
}

async fn load_checkpoint(output: &Path) -> ControlFile {
    ControlFile::load(&ControlFile::control_path_for(output))
        .await
        .expect("read control file")
        .expect("control file exists")
}

#[tokio::test]
async fn sequential_retry_resumes_from_where_the_broken_attempt_stopped() {
    let data = payload(64 * KIB);
    let broken_at = 40_000;
    let rest = &data[broken_at..];
    let (uri, server) = serve(vec![
        response("HTTP/1.1 200 OK", &data, broken_at),
        response(
            &format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {broken_at}-{}/{}",
                data.len() - 1,
                data.len()
            ),
            rest,
            rest.len(),
        ),
    ])
    .await;
    let dir = tempfile::tempdir().expect("create temporary download directory");
    let output = dir.path().join("payload.bin");
    let mut downloader = make_downloader(&uri, output.clone(), 9_910);
    let fresh = ResumeHelper::new(&output, false)
        .detect(data.len() as u64)
        .await
        .expect("detect fresh download");

    // A total length of zero keeps the Linux splice shortcut out of the way;
    // the response provides the length.
    let result = downloader
        .execute_with_retry(&uri, &fresh, 0, &RetryPolicy::new(2, 0))
        .await;
    let requests = server.await.expect("test HTTP server should finish");

    assert!(result.is_ok(), "sequential retry failed: {result:?}");
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].contains(&format!("range: bytes={broken_at}-\r\n")),
        "the retry restarted instead of resuming: {}",
        requests[1]
    );
    assert_eq!(tokio::fs::read(&output).await.expect("read output"), data);
}

#[tokio::test]
async fn sequential_progress_over_a_split_checkpoint_marks_pieces() {
    let piece = 64 * KIB;
    let data = payload(4 * piece);
    let dir = tempfile::tempdir().expect("create temporary download directory");
    let output = dir.path().join("payload.bin");
    split_checkpoint(&output, &data, piece, &[0, 3]).await;
    let state = ResumeHelper::new(&output, true)
        .detect(data.len() as u64)
        .await
        .expect("detect split checkpoint");
    assert_eq!(state.start_offset, piece as u64);

    // The stream breaks 20 KiB into piece 2.
    let rest = &data[piece..];
    let (uri, server) = serve(vec![response(
        &format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {piece}-{}/{}",
            data.len() - 1,
            data.len()
        ),
        rest,
        piece + 20 * KIB,
    )])
    .await;
    let mut downloader = make_downloader(&uri, output.clone(), 9_911);
    let result = downloader.execute(&uri, &state, data.len() as u64).await;
    server.await.expect("test HTTP server should finish");
    assert!(result.is_err(), "a broken stream must fail the attempt");

    let control_file = load_checkpoint(&output).await;
    assert!(control_file.is_piece_done(0));
    assert!(
        control_file.is_piece_done(1),
        "a concurrent resume would download piece 1 again"
    );
    assert!(!control_file.is_piece_done(2));
    assert!(control_file.is_piece_done(3));
    assert_eq!(
        control_file
            .in_flight_pieces()
            .iter()
            .map(|piece| piece.index)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let file = tokio::fs::read(&output).await.expect("read output");
    assert_eq!(&file[..2 * piece + 20 * KIB], &data[..2 * piece + 20 * KIB]);
    assert_eq!(&file[3 * piece..], &data[3 * piece..]);
}

#[tokio::test]
async fn sequential_resume_from_offset_zero_keeps_finished_pieces_on_disk() {
    let piece = 64 * KIB;
    let data = payload(4 * piece);
    let dir = tempfile::tempdir().expect("create temporary download directory");
    let output = dir.path().join("payload.bin");
    split_checkpoint(&output, &data, piece, &[2]).await;
    let state = ResumeHelper::new(&output, true)
        .detect(data.len() as u64)
        .await
        .expect("detect split checkpoint");
    assert!(state.should_resume);
    assert_eq!(state.start_offset, 0);

    let (uri, server) = serve(vec![response("HTTP/1.1 200 OK", &data, 20 * KIB)]).await;
    let mut downloader = make_downloader(&uri, output.clone(), 9_912);
    let result = downloader.execute(&uri, &state, data.len() as u64).await;
    server.await.expect("test HTTP server should finish");
    assert!(result.is_err(), "a broken stream must fail the attempt");

    // The checkpoint still lists piece 2, so its bytes must still be there.
    let control_file = load_checkpoint(&output).await;
    assert!(control_file.is_piece_done(2));
    let file = tokio::fs::read(&output).await.expect("read output");
    assert!(file.len() >= 3 * piece, "the output was truncated");
    assert_eq!(&file[2 * piece..3 * piece], &data[2 * piece..3 * piece]);
}

#[tokio::test]
async fn gap_progress_is_recorded_in_the_split_checkpoint() {
    let piece = 32 * KIB;
    let data = payload(4 * piece);
    let dir = tempfile::tempdir().expect("create temporary download directory");
    let output = dir.path().join("payload.bin");
    split_checkpoint(&output, &data, piece, &[0]).await;

    // The gap is pieces 1..4; its response ends 28 KiB into piece 2.
    let gap = &data[piece..];
    let (uri, server) = serve(vec![response(
        &format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {piece}-{}/{}",
            data.len() - 1,
            data.len()
        ),
        gap,
        piece + 28 * KIB,
    )])
    .await;
    let mut downloader = make_downloader(&uri, output.clone(), 9_913);
    let result = downloader
        .execute_with_gaps(&uri, data.len() as u64, &[(0, piece as u64)])
        .await;
    server.await.expect("test HTTP server should finish");
    assert!(result.error.is_some(), "a short gap must fail the attempt");

    let control_file = load_checkpoint(&output).await;
    assert!(control_file.is_piece_done(0));
    assert!(
        control_file.is_piece_done(1),
        "the next run would fetch piece 1 again"
    );
    assert!(!control_file.is_piece_done(2));
    assert_eq!(
        control_file
            .in_flight_pieces()
            .iter()
            .map(|piece| (piece.index, piece.bitfield.clone()))
            .collect::<Vec<_>>(),
        vec![(2, vec![0b1000_0000])]
    );
    let file = tokio::fs::read(&output).await.expect("read output");
    assert_eq!(&file[..2 * piece + 16 * KIB], &data[..2 * piece + 16 * KIB]);
}

#[tokio::test]
async fn gap_after_an_unaligned_in_flight_prefix_finishes_its_piece() {
    let piece = 32 * KIB;
    let data = payload(2 * piece);
    let dir = tempfile::tempdir().expect("create temporary download directory");
    let output = dir.path().join("payload.bin");
    split_checkpoint(&output, &data, piece, &[]).await;
    // The split attempt wrote 20 KiB of piece 0, more than the whole 16 KiB
    // blocks a checkpoint can hold; the fallback gets the exact 20 KiB.
    let written = 20 * KIB;
    let mut file = tokio::fs::read(&output).await.expect("read output");
    file[..written].copy_from_slice(&data[..written]);
    tokio::fs::write(&output, file)
        .await
        .expect("write partial piece");

    let gap = &data[written..];
    // The gap response ends 4 KiB into piece 1.
    let (uri, server) = serve(vec![response(
        &format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {written}-{}/{}",
            data.len() - 1,
            data.len()
        ),
        gap,
        piece - written + 4 * KIB,
    )])
    .await;
    let mut downloader = make_downloader(&uri, output.clone(), 9_914);
    let result = downloader
        .execute_with_gaps(&uri, data.len() as u64, &[(0, written as u64)])
        .await;
    server.await.expect("test HTTP server should finish");
    assert!(result.error.is_some(), "a short gap must fail the attempt");

    let control_file = load_checkpoint(&output).await;
    assert!(
        control_file.is_piece_done(0),
        "the gap finished piece 0 after its unaligned prefix"
    );
}
