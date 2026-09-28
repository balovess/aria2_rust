use super::*;

#[tokio::test]
async fn resumed_download_reuses_content_disposition_path_with_control_file() {
    use crate::filesystem::control_file::ControlFile;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind resume filename fixture");
    let address = listener
        .local_addr()
        .expect("read resume filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept resume filename request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read resume filename request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            let request_lower = request.to_ascii_lowercase();
            if request_index == 0 {
                assert!(!request_lower.contains("range: bytes="));
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\nabcd",
                    )
                    .await
                    .expect("write resume metadata response");
            } else {
                assert!(request_lower.contains("range: bytes=2-"));
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 2-3/4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\ncd",
                    )
                    .await
                    .expect("write resume range response");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create resume output directory");
    let output = directory.path().join("actual.txt");
    tokio::fs::write(&output, b"ab")
        .await
        .expect("create partial output");
    let control_path = ControlFile::control_path_for(&output);
    let mut control = ControlFile::open_or_create(&control_path, 4, 1)
        .await
        .expect("create resume control file");
    control.update_completed_length(2);
    control.save().await.expect("save resume control file");

    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        continue_download: true,
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1007), &uri, &options, None, None)
        .expect("create resume filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("resume filename download should not hang")
        .expect("resume filename download should complete");

    assert_eq!(tokio::fs::read(&output).await.unwrap(), b"abcd");
    assert!(!directory.path().join("actual.1.txt").exists());
    server.await.expect("resume filename fixture should finish");
}

#[tokio::test]
async fn mirror_resume_failure_reuses_resolved_output_path() {
    use crate::filesystem::control_file::ControlFile;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let first_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind first mirror fixture");
    let first_address = first_listener
        .local_addr()
        .expect("read first mirror fixture address");
    let first_server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut stream, _) = first_listener
                .accept()
                .await
                .expect("accept first mirror request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read first mirror request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            let request_lower = request.to_ascii_lowercase();

            if request_index == 0 {
                assert!(request.starts_with("GET /download.bin HTTP/1.1\r\n"));
                assert!(!request_lower.contains("range: bytes="));
            } else {
                assert!(request.starts_with("GET /download.bin HTTP/1.1\r\n"));
                assert!(request_lower.contains("range: bytes=2-"));
            }

            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=first.txt\r\nConnection: close\r\n\r\nabcd",
                )
                .await
                .expect("write first mirror response");
        }
    });

    let second_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind second mirror fixture");
    let second_address = second_listener
        .local_addr()
        .expect("read second mirror fixture address");
    let second_server = tokio::spawn(async move {
        let (mut stream, _) = second_listener
            .accept()
            .await
            .expect("accept second mirror request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read second mirror request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        let request_lower = request.to_ascii_lowercase();
        assert!(request.starts_with("GET /download.bin HTTP/1.1\r\n"));
        assert!(request_lower.contains("range: bytes=2-"));
        stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 2-3/4\r\nContent-Disposition: attachment; filename=second.txt\r\nConnection: close\r\n\r\ncd",
            )
            .await
            .expect("write second mirror response");
    });

    let directory = tempfile::tempdir().expect("create mirror resume output directory");
    let output = directory.path().join("first.txt");
    tokio::fs::write(&output, b"ab")
        .await
        .expect("create mirror resume partial output");
    let control_path = ControlFile::control_path_for(&output);
    let mut control = ControlFile::open_or_create(&control_path, 4, 1)
        .await
        .expect("create mirror resume control file");
    control.update_completed_length(2);
    control
        .save()
        .await
        .expect("save mirror resume control file");

    let first_uri = format!("http://{first_address}/download.bin");
    let second_uri = format!("http://{second_address}/download.bin");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        continue_download: true,
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let group = std::sync::Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1010),
        vec![first_uri.clone(), second_uri],
        options.clone(),
    )));
    let mut command = DownloadCommand::new_with_group(group, &first_uri, &options, None, None)
        .expect("create mirror resume command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("mirror resume download should not hang")
        .expect("mirror resume download should complete");

    assert_eq!(tokio::fs::read(&output).await.unwrap(), b"abcd");
    assert!(!directory.path().join("first.1.txt").exists());
    assert!(!directory.path().join("second.txt").exists());
    first_server
        .await
        .expect("first mirror fixture should finish");
    second_server
        .await
        .expect("second mirror fixture should finish");
}

#[tokio::test]
async fn cannot_resume_falls_back_to_fresh_download_without_renaming_path() {
    use crate::filesystem::control_file::ControlFile;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind cannot-resume filename fixture");
    let address = listener
        .local_addr()
        .expect("read cannot-resume filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept cannot-resume filename request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read cannot-resume filename request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            let request_lower = request.to_ascii_lowercase();
            match request_index {
                0 => {
                    assert!(request.starts_with("HEAD /download HTTP/1.1\r\n"));
                    assert!(!request_lower.contains("range: bytes="));
                }
                1 => assert!(request_lower.contains("range: bytes=2-")),
                2 => {
                    assert!(request.starts_with("HEAD /download HTTP/1.1\r\n"));
                    assert!(!request_lower.contains("range: bytes="));
                }
                3 => {
                    assert!(request.starts_with("GET /download HTTP/1.1\r\n"));
                    assert!(!request_lower.contains("range: bytes="));
                }
                _ => unreachable!(),
            }
            stream
                .write_all(
                    if matches!(request_index, 0 | 2) {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\n".as_slice()
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\nabcd".as_slice()
                    },
                )
                .await
                .expect("write cannot-resume response");
        }
    });

    let directory = tempfile::tempdir().expect("create cannot-resume output directory");
    let output = directory.path().join("actual.txt");
    tokio::fs::write(&output, b"ab")
        .await
        .expect("create cannot-resume partial output");
    let control_path = ControlFile::control_path_for(&output);
    let mut control = ControlFile::open_or_create(&control_path, 4, 1)
        .await
        .expect("create cannot-resume control file");
    control.update_completed_length(2);
    control
        .save()
        .await
        .expect("save cannot-resume control file");

    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        continue_download: true,
        force_sequential: true,
        use_head: true,
        always_resume: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1008), &uri, &options, None, None)
        .expect("create cannot-resume filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("cannot-resume filename download should not hang")
        .expect("cannot-resume filename download should complete fresh");

    assert_eq!(tokio::fs::read(&output).await.unwrap(), b"abcd");
    assert!(!directory.path().join("actual.1.txt").exists());
    server
        .await
        .expect("cannot-resume filename fixture should finish");
}

#[tokio::test]
async fn invalid_content_disposition_falls_back_to_url_filename() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind invalid filename fixture");
    let address = listener
        .local_addr()
        .expect("read invalid filename fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept invalid filename request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read invalid filename request");
        assert!(
            String::from_utf8_lossy(&request[..bytes])
                .starts_with("GET /fallback.bin HTTP/1.1\r\n")
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=../escape.bin\r\nConnection: close\r\n\r\ndata",
            )
            .await
            .expect("write invalid filename response");
    });

    let directory = tempfile::tempdir().expect("create invalid filename directory");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/fallback.bin");
    let mut command = DownloadCommand::new(GroupId::new(1012), &uri, &options, None, None)
        .expect("create invalid filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("invalid filename download should not hang")
        .expect("invalid filename download should complete");

    assert_eq!(
        tokio::fs::read(directory.path().join("fallback.bin"))
            .await
            .expect("read URL fallback output"),
        b"data"
    );
    assert!(!directory.path().join("escape.bin").exists());
    server
        .await
        .expect("invalid filename fixture should finish");
}

#[tokio::test]
async fn reserved_content_disposition_name_is_sanitized_before_download() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind reserved filename fixture");
    let address = listener
        .local_addr()
        .expect("read reserved filename fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept reserved filename request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read reserved filename request");
        assert!(
            String::from_utf8_lossy(&request[..bytes]).starts_with("GET /download HTTP/1.1\r\n")
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=CON.txt\r\nConnection: close\r\n\r\ndata",
            )
            .await
            .expect("write reserved filename response");
    });

    let directory = tempfile::tempdir().expect("create reserved filename directory");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1013), &uri, &options, None, None)
        .expect("create reserved filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("reserved filename download should not hang")
        .expect("reserved filename download should complete");

    assert_eq!(
        tokio::fs::read(directory.path().join("_CON.txt"))
            .await
            .expect("read sanitized reserved output"),
        b"data"
    );
    assert!(!directory.path().join("CON.txt").exists());
    server
        .await
        .expect("reserved filename fixture should finish");
}

#[tokio::test]
async fn empty_control_and_long_content_disposition_names_are_safe_on_disk() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind filename edge-case fixture");
    let address = listener
        .local_addr()
        .expect("read filename edge-case fixture address");
    let encoded_long_name = crate::util::uri::percent_encode(&"界".repeat(100));
    let server = tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept filename edge-case request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read filename edge-case request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            let expected_path = match request_index {
                0 => "/empty.bin",
                1 => "/control.bin",
                2 => "/long.bin",
                _ => unreachable!(),
            };
            assert!(request.starts_with(&format!("GET {expected_path} HTTP/1.1\r\n")));

            let response = match request_index {
                0 => b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Disposition: attachment; filename=\"\"\r\nConnection: close\r\n\r\none".to_vec(),
                1 => b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Disposition: attachment; filename*=UTF-8''bad%00name.txt\r\nConnection: close\r\n\r\ntwo".to_vec(),
                2 => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Disposition: attachment; filename*=UTF-8''{encoded_long_name}\r\nConnection: close\r\n\r\ntri"
                )
                .into_bytes(),
                _ => unreachable!(),
            };
            stream
                .write_all(&response)
                .await
                .expect("write filename edge-case response");
        }
    });

    let directory = tempfile::tempdir().expect("create filename edge-case directory");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let cases = [
        (1014, "empty.bin", "empty.bin".to_owned(), b"one".as_slice()),
        (
            1015,
            "control.bin",
            "control.bin".to_owned(),
            b"two".as_slice(),
        ),
        (1016, "long.bin", "界".repeat(85), b"tri".as_slice()),
    ];
    for (gid, uri_path, output_name, expected_body) in cases {
        let uri = format!("http://{address}/{uri_path}");
        let mut command = DownloadCommand::new(GroupId::new(gid), &uri, &options, None, None)
            .expect("create filename edge-case command");
        tokio::time::timeout(Duration::from_secs(5), command.execute())
            .await
            .expect("filename edge-case download should not hang")
            .expect("filename edge-case download should complete");
        assert_eq!(
            tokio::fs::read(directory.path().join(output_name))
                .await
                .expect("read URL fallback filename"),
            expected_body
        );
    }

    assert!(!directory.path().join("bad").exists());
    assert!(!directory.path().join("name.txt").exists());
    server
        .await
        .expect("filename edge-case fixture should finish");
}

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
