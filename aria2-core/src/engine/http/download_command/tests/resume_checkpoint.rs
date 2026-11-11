use super::*;

#[tokio::test]
async fn prepared_get_uses_preemptive_credentials_for_filename_metadata() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind authenticated filename fixture");
    let address = listener
        .local_addr()
        .expect("read authenticated filename fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept authenticated filename request");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let bytes = stream
                .read(&mut chunk)
                .await
                .expect("read authenticated filename request");
            assert!(bytes > 0, "authenticated request ended before its headers");
            request.extend_from_slice(&chunk[..bytes]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        assert!(request.starts_with("GET /download HTTP/1.1\r\n"));
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Basic dXNlcjpwYXNz")),
            "request did not contain preemptive credentials: {request}"
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=auth.txt\r\nConnection: close\r\n\r\ndata",
            )
            .await
            .expect("write authenticated GET response");
    });

    let directory = tempfile::tempdir().expect("create authenticated output directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1006), &uri, &options, None, None)
        .expect("create authenticated filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("authenticated filename download should not hang")
        .expect("authenticated filename download should complete");

    assert_eq!(
        std::fs::read(directory.path().join("auth.txt")).expect("read authenticated filename"),
        b"data"
    );
    server
        .await
        .expect("authenticated filename fixture should finish");
}

#[tokio::test]
async fn sequential_resume_from_split_checkpoint_stops_at_first_gap() {
    use crate::filesystem::control_file::ControlFile;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind split checkpoint fixture");
    let address = listener
        .local_addr()
        .expect("read split checkpoint fixture address");
    let expected: Vec<u8> = (0..1000).map(|index| (index % 251) as u8).collect();
    let response_body = expected[100..].to_vec();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept split checkpoint request");
        let request = read_http_request(&mut stream).await;
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("range: bytes=100-")),
            "sequential resume must stop at the first unfinished piece: {request:?}"
        );

        let mut response = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 100-999/1000\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            response_body.len()
        )
        .into_bytes();
        response.extend_from_slice(&response_body);
        stream
            .write_all(&response)
            .await
            .expect("write split checkpoint response");
    });

    let directory = tempfile::tempdir().expect("create split checkpoint output directory");
    let output = directory.path().join("split-gap.bin");
    tokio::fs::write(&output, &expected)
        .await
        .expect("create split checkpoint output");
    let control_path = ControlFile::control_path_for(&output);
    let mut control = ControlFile::open_or_create_with_piece_length(&control_path, 1000, 100)
        .await
        .expect("create split control file");
    control.mark_piece_done(0);
    control.mark_piece_done(2);
    control.save().await.expect("save split control file");

    let options = DownloadOptions {
        continue_download: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/split-gap.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(1017),
        &uri,
        &options,
        directory.path().to_str(),
        Some("split-gap.bin"),
    )
    .expect("create sequential split checkpoint command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("split checkpoint resume should not hang")
        .expect("split checkpoint resume should complete");

    assert_eq!(tokio::fs::read(&output).await.unwrap(), expected);
    server
        .await
        .expect("split checkpoint fixture should finish");
}
