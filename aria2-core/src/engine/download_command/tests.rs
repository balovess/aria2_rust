use std::sync::Arc;
use std::time::Duration;

use crate::engine::command::{Command, ProgressUpdate};
use crate::engine::download_command::DownloadCommand;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError};
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::{DownloadOptions, FollowMode, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

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
                .try_send(update)
                .expect("progress test channel should accept the update");
        } else {
            panic!("test called send_progress_update but no sender is set");
        }
    }
}

#[test]
fn command_timeout_comes_from_download_options() {
    let options = DownloadOptions {
        timeout: Some(7),
        ..DownloadOptions::default()
    };
    let command = DownloadCommand::new(
        GroupId::new(1001),
        "http://example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("HTTP command should accept a valid URI");

    assert_eq!(
        Command::timeout(&command),
        Some(Duration::from_secs(7)),
        "timeout must be the configured I/O inactivity duration"
    );
}

#[test]
fn inferred_http_output_name_uses_the_safe_decoded_url_segment() {
    let options = DownloadOptions::default();
    let command = DownloadCommand::new(
        GroupId::new(1003),
        "https://example.com/releases/my%20file.zip?token=ignored#fragment",
        &options,
        None,
        None,
    )
    .expect("HTTP command should accept a valid URI");

    assert_eq!(
        command
            .output_path
            .file_name()
            .and_then(|name| name.to_str()),
        Some("my file.zip")
    );
}

#[tokio::test]
async fn head_content_disposition_replaces_an_inferred_http_output_name() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind filename fixture");
    let address = listener
        .local_addr()
        .expect("read filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut stream, _) = listener.accept().await.expect("accept filename request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read filename request");
            let request = String::from_utf8_lossy(&request[..bytes]);

            if request_index == 0 {
                assert!(request.starts_with("HEAD /download HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write HEAD redirect response");
            } else if request_index == 1 {
                assert!(request.starts_with("HEAD /final HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename*=UTF-8''server%20name.txt\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write final HEAD response");
            } else if request_index == 2 {
                assert!(request.starts_with("GET /download HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write GET redirect response");
            } else {
                assert!(request.starts_with("GET /final HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata",
                    )
                    .await
                    .expect("write GET response");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create filename output directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        use_head: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1004), &uri, &options, None, None)
        .expect("create inferred filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("filename download should not hang")
        .expect("filename download should complete");

    assert_eq!(
        std::fs::read(directory.path().join("server name.txt")).expect("read server filename"),
        b"data"
    );
    assert!(!directory.path().join("download").exists());
    server.await.expect("filename fixture should finish");
}

#[tokio::test]
async fn first_get_content_disposition_is_reused_for_sequential_download() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind filename fixture");
    let address = listener
        .local_addr()
        .expect("read filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept filename request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read filename request");
            let request = String::from_utf8_lossy(&request[..bytes]);

            if request_index == 0 {
                assert!(request.starts_with("GET /download HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write GET redirect response");
            } else {
                assert!(request.starts_with("GET /final HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\ndata",
                    )
                    .await
                    .expect("write GET response");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create filename output directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1005), &uri, &options, None, None)
        .expect("create inferred filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("filename download should not hang")
        .expect("filename download should complete");

    assert_eq!(
        std::fs::read(directory.path().join("actual.txt")).expect("read server filename"),
        b"data"
    );
    assert!(!directory.path().join("download").exists());
    server.await.expect("filename fixture should finish");
}

#[tokio::test]
async fn inferred_content_disposition_name_enters_collision_policy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind collision filename fixture");
    let address = listener
        .local_addr()
        .expect("read collision filename fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept collision filename request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read collision filename request");
        assert!(
            String::from_utf8_lossy(&request[..bytes]).starts_with("GET /download HTTP/1.1\r\n")
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\ndata",
            )
            .await
            .expect("write collision filename response");
    });

    let directory = tempfile::tempdir().expect("create collision output directory");
    let existing = directory.path().join("actual.txt");
    std::fs::write(&existing, b"keep").expect("create existing output");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1006), &uri, &options, None, None)
        .expect("create collision filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("collision filename download should not hang")
        .expect("collision filename download should complete");

    assert_eq!(
        std::fs::read(&existing).expect("read existing output"),
        b"keep"
    );
    assert_eq!(
        std::fs::read(directory.path().join("actual.1.txt")).expect("read renamed output"),
        b"data"
    );
    server
        .await
        .expect("collision filename fixture should finish");
}

#[tokio::test]
async fn redirected_content_disposition_name_enters_collision_policy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirected collision filename fixture");
    let address = listener
        .local_addr()
        .expect("read redirected collision filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept redirected collision filename request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read redirected collision filename request");
            let request = String::from_utf8_lossy(&request[..bytes]);

            if request_index == 0 {
                assert!(request.starts_with("GET /download HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write redirected collision response");
            } else {
                assert!(request.starts_with("GET /final HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\ndata",
                    )
                    .await
                    .expect("write redirected collision final response");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create redirected collision output directory");
    let existing = directory.path().join("actual.txt");
    std::fs::write(&existing, b"keep").expect("create redirected existing output");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1009), &uri, &options, None, None)
        .expect("create redirected collision filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("redirected collision filename download should not hang")
        .expect("redirected collision filename download should complete");

    assert_eq!(
        std::fs::read(&existing).expect("read redirected existing output"),
        b"keep"
    );
    assert_eq!(
        std::fs::read(directory.path().join("actual.1.txt"))
            .expect("read redirected renamed output"),
        b"data"
    );
    server
        .await
        .expect("redirected collision filename fixture should finish");
}

#[tokio::test]
async fn explicit_output_name_overrides_content_disposition() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind explicit output fixture");
    let address = listener
        .local_addr()
        .expect("read explicit output fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept explicit output request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read explicit output request");
        assert!(
            String::from_utf8_lossy(&request[..bytes]).starts_with("GET /download HTTP/1.1\r\n")
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=server.txt\r\nConnection: close\r\n\r\ndata",
            )
            .await
            .expect("write explicit output response");
    });

    let directory = tempfile::tempdir().expect("create explicit output directory");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command =
        DownloadCommand::new(GroupId::new(1011), &uri, &options, None, Some("chosen.bin"))
            .expect("create explicit output command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("explicit output download should not hang")
        .expect("explicit output download should complete");

    assert_eq!(
        tokio::fs::read(directory.path().join("chosen.bin"))
            .await
            .expect("read explicit output"),
        b"data"
    );
    assert!(!directory.path().join("server.txt").exists());
    server.await.expect("explicit output fixture should finish");
}

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

#[test]
fn force_sequential_disables_concurrent_range_downloads() {
    let options = DownloadOptions {
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let command = DownloadCommand::new(
        GroupId::new(1002),
        "http://example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("HTTP command should accept a valid URI");

    assert!(!command.should_use_concurrent(16 * 1024 * 1024, true, 4));
}

#[test]
fn in_memory_metadata_retry_classification_matches_http_contract() {
    let policy = RetryPolicy::new(2, 0);
    let server_error = |code| Aria2Error::Recoverable(RecoverableError::ServerError { code });

    assert!(super::execute::should_retry_in_memory_error(
        &server_error(504),
        0,
        &policy,
        0,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &server_error(504),
        1,
        &policy,
        0,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &server_error(500),
        0,
        &policy,
        1,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &server_error(502),
        0,
        &policy,
        0,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &server_error(502),
        0,
        &policy,
        1,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &server_error(503),
        0,
        &policy,
        1,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: "HTTP error: 429".to_string(),
        }),
        0,
        &policy,
        1,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "connection reset".to_string(),
        }),
        0,
        &policy,
        0,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::Timeout),
        0,
        &policy,
        0,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::ResourceNotFound),
        0,
        &policy,
        0,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::ResourceNotFound),
        0,
        &policy,
        0,
        true,
    ));
}

#[tokio::test]
async fn in_memory_http_records_each_payload_chunk_for_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let first_chunk = vec![b'A'; 16 * 1024];
    let second_chunk = vec![b'B'; 16 * 1024];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let first_chunk = first_chunk.clone();
        let second_chunk = second_chunk.clone();
        async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let bytes_read = stream.read(&mut request).await.unwrap();
            assert!(bytes_read > 0, "HTTP fixture should receive a request");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        first_chunk.len() + second_chunk.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&first_chunk).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            stream.write_all(&second_chunk).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });

    let url = format!("http://{address}/metadata.torrent");
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9001),
        vec![url.clone()],
        options.clone(),
    )));
    let mut command =
        DownloadCommand::new_with_group(Arc::clone(&group), &url, &options, None, None).unwrap();

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if group.recover().completed_length() >= first_chunk.len() as u64 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("in-memory HTTP did not receive its first payload chunk");
    let first_activity = group.recover().last_network_activity();

    tokio::time::timeout(Duration::from_secs(5), command_task)
        .await
        .expect("in-memory HTTP command did not complete")
        .expect("in-memory HTTP command panicked")
        .expect("in-memory HTTP command failed");
    server.await.unwrap();

    assert!(
        group.recover().last_network_activity() > first_activity,
        "each non-empty in-memory HTTP chunk must refresh the inactivity clock"
    );
}

#[tokio::test]
async fn interface_binding_reaches_the_http_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("HTTP fixture should bind");
    let address = listener.local_addr().expect("HTTP fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("HTTP client should connect");
        let mut request = [0u8; 4096];
        let bytes = stream.read(&mut request).await.expect("read HTTP request");
        assert!(bytes > 0, "HTTP request should not be empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("write HTTP response");
        peer.ip()
    });

    let url = format!("http://{address}/bound.bin");
    let options = DownloadOptions {
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9101),
        vec![url.clone()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group_and_resolved_addresses_and_policy(
        group,
        &url,
        &options,
        None,
        None,
        None,
        Arc::new(OutboundNetworkPolicy::single(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        ))),
    )
    .expect("HTTP command should build with an interface binding");

    let response = command
        .client
        .get(&url)
        .send()
        .await
        .expect("bound HTTP client should connect");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        server.await.expect("HTTP fixture should finish"),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[test]
fn test_progress_channel_auto_created() {
    let cmd = DownloadCommand::new(
        GroupId::new(1),
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new should succeed with a valid HTTP URI");

    assert!(
        cmd.has_progress_sender(),
        "progress_sender should be Some after construction (auto-created)"
    );
    assert!(
        cmd.has_progress_receiver(),
        "progress_receiver should be Some after construction (held for lazy spawn)"
    );
    assert!(
        !cmd.has_progress_aggregator_handle(),
        "progress_aggregator_handle should be None until execute() spawns it"
    );
}

#[test]
fn primary_http_client_applies_custom_tls_configuration() {
    let directory = tempfile::tempdir().expect("create temporary TLS configuration directory");
    let ca_path = directory.path().join("ca.pem");
    std::fs::write(&ca_path, b"not a CA certificate").expect("write invalid CA fixture");

    let options = DownloadOptions {
        ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
        ..DownloadOptions::default()
    };
    let error = match DownloadCommand::new(
        GroupId::new(3),
        "https://example.com/file.bin",
        &options,
        None,
        None,
    ) {
        Ok(_) => panic!("invalid custom CA configuration must reject the primary client"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("Invalid CA certificate"));
}

#[test]
fn primary_http_client_builds_with_certificate_verification_disabled() {
    let options = DownloadOptions {
        check_certificate: false,
        ..DownloadOptions::default()
    };

    DownloadCommand::new(
        GroupId::new(4),
        "https://example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("verification-disabled TLS configuration should build the client");
}

#[tokio::test]
async fn test_progress_updates_flow_through_channel() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));
    let group_clone = Arc::clone(&group);

    let mut cmd = DownloadCommand::new_with_group(
        group,
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    assert!(cmd.has_progress_sender());
    assert!(cmd.has_progress_receiver());

    cmd.spawn_progress_aggregator();
    assert!(cmd.has_progress_aggregator_handle());
    assert!(!cmd.has_progress_receiver());

    cmd.send_progress_update(ProgressUpdate {
        completed_bytes: 4096,
        download_speed: 0,
        upload_speed: 0,
    });

    cmd.drain_progress_aggregator().await;
    assert!(!cmd.has_progress_sender());
    assert!(!cmd.has_progress_aggregator_handle());

    let completed = { group_clone.recover().get_completed_length() };
    assert_eq!(
        completed, 4096,
        "aggregator should have applied the progress update to RequestGroup"
    );
}

/// Verify that check_cancelled() returns Ok(()) for a fresh group
/// (status = Waiting) and Err(DownloadFailed) after the group is
/// marked Removed.
#[tokio::test]
async fn test_check_cancelled_returns_ok_for_active_group() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(10),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));

    let cmd = DownloadCommand::new_with_group(
        group,
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    // Fresh group (Waiting status) -- not cancelled.
    assert!(
        cmd.check_cancelled().is_ok(),
        "check_cancelled() should return Ok for a fresh (non-removed) group"
    );
}

#[tokio::test]
async fn test_check_cancelled_returns_err_after_remove() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(11),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));

    let cmd = DownloadCommand::new_with_group(
        Arc::clone(&group),
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    // Simulate aria2.remove / aria2.forceRemove which calls
    // RequestGroupMan::remove_group -> group.remove().
    {
        let mut g = group.recover_mut();
        g.remove().unwrap();
    }

    let err = cmd
        .check_cancelled()
        .expect_err("check_cancelled() should return Err after the group is marked Removed");
    assert!(
        matches!(err, Aria2Error::DownloadFailed(_)),
        "expected DownloadFailed error, got {:?}",
        err
    );
}

#[tokio::test]
async fn test_retry_wait_is_interruptible_when_paused() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(12),
        vec!["http://example.com/metadata.torrent".to_string()],
        DownloadOptions::default(),
    )));
    let command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        "http://example.com/metadata.torrent",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");
    group.recover_mut().pause().unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        command.wait_for_retry(std::time::Duration::from_secs(5)),
    )
    .await
    .expect("paused retry wait should stop promptly");

    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}

#[tokio::test]
async fn test_retry_wait_wakes_when_paused_after_wait_starts() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(13),
        vec!["http://example.com/metadata.torrent".to_string()],
        DownloadOptions::default(),
    )));
    let command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        "http://example.com/metadata.torrent",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");
    let wait_task = tokio::spawn(async move {
        command
            .wait_for_retry(std::time::Duration::from_secs(5))
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    group.recover_mut().pause().unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), wait_task)
        .await
        .expect("pause should wake an active metadata retry wait")
        .expect("metadata retry wait task should not panic");
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}

#[tokio::test]
async fn proxy_client_leaves_redirects_for_the_download_flow() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local proxy fixture");
    let proxy_addr = listener.local_addr().expect("read proxy address");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests_for_server = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for request_number in 1..=2 {
            let accepted = if request_number == 1 {
                Some(
                    listener
                        .accept()
                        .await
                        .expect("accept initial proxy request"),
                )
            } else {
                tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept())
                    .await
                    .ok()
                    .map(|result| result.expect("accept redirected proxy request"))
            };
            let Some((mut stream, _)) = accepted else {
                break;
            };
            requests_for_server.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut request = vec![0; 4096];
            let bytes = stream.read(&mut request).await.expect("read proxy request");
            assert!(bytes > 0, "proxy request should not be empty");
            let response = if request_number == 1 {
                b"HTTP/1.1 302 Found\r\nLocation: http://origin.example/redirect-target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            };
            stream
                .write_all(response)
                .await
                .expect("write proxy response");
        }
    });

    let options = DownloadOptions {
        http_proxy: Some(format!("http://{proxy_addr}")),
        ..DownloadOptions::default()
    };
    let command = DownloadCommand::new(
        GroupId::new(12),
        "http://origin.example/file.bin",
        &options,
        None,
        None,
    )
    .expect("create proxied download command");

    let response = command
        .client
        .get("http://origin.example/file.bin")
        .send()
        .await
        .expect("proxy should return the redirect response");
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "proxy redirects must be handled by SequentialDownloader so URI and retry state stay canonical"
    );

    server.await.expect("proxy fixture should finish");
}

#[tokio::test]
async fn proxied_client_binds_source_for_the_proxy_peer() {
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind source-aware proxy fixture");
    let proxy_addr = listener
        .local_addr()
        .expect("read source-aware proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener
            .accept()
            .await
            .expect("accept source-aware proxy request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read source-aware proxy request");
        assert!(bytes > 0, "proxy request should not be empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("write source-aware proxy response");
        peer
    });

    let options = DownloadOptions {
        http_proxy: Some(format!("http://{proxy_addr}")),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1201),
        vec!["http://[::1]:9/file.bin".to_string()],
        options.clone(),
    )));
    let policy = Arc::new(
        OutboundNetworkPolicy::new(vec![
            IpAddr::V6("::1".parse().expect("parse IPv6 source")),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
        ])
        .expect("source policy should accept both address families"),
    );
    let command = DownloadCommand::new_with_group_and_resolved_addresses_and_policy(
        group,
        "http://[::1]:9/file.bin",
        &options,
        None,
        None,
        None,
        policy,
    )
    .expect("proxy client must select a source compatible with the proxy peer");

    let response = command
        .client
        .get("http://[::1]:9/file.bin")
        .send()
        .await
        .expect("proxy should serve the IPv6-target request");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        server.await.expect("proxy fixture should finish").ip(),
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))
    );
}

#[tokio::test]
async fn authentication_retry_follows_redirect_and_preserves_protection_space() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth redirect fixture");
    let address = listener.local_addr().expect("read auth redirect address");
    let server = tokio::spawn(async move {
        for request_number in 1..=3 {
            let (mut stream, _) = listener.accept().await.expect("accept auth request");
            let mut request = vec![0; 4096];
            let bytes = stream.read(&mut request).await.expect("read auth request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            let has_authorization = request.lines().any(|line| {
                line.to_ascii_lowercase()
                    .starts_with("authorization: basic ")
            });

            match request_number {
                1 => {
                    assert!(request.starts_with("GET /protected/file.bin HTTP/1.1\r\n"));
                    assert!(
                        !has_authorization,
                        "initial request must be unauthenticated"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth challenge");
                }
                2 => {
                    assert!(request.starts_with("GET /protected/file.bin HTTP/1.1\r\n"));
                    assert!(has_authorization, "auth retry must include credentials");
                    stream
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /protected/final.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth redirect");
                }
                3 => {
                    assert!(request.starts_with("GET /protected/final.bin HTTP/1.1\r\n"));
                    assert!(
                        has_authorization,
                        "same-host redirect must preserve the activated protection space"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nContent-Disposition: attachment; filename=authenticated.bin\r\nConnection: close\r\n\r\nauth-redirect\n",
                        )
                        .await
                        .expect("write final authenticated response");
                }
                _ => unreachable!(),
            }
        }
    });

    let directory = tempfile::tempdir().expect("create auth redirect directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("password".to_string()),
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/protected/file.bin");
    let output = directory.path().join("authenticated.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(15),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        None,
    )
    .expect("create auth redirect command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("auth redirect download should not hang")
        .expect("auth retry redirect should complete");
    assert_eq!(
        std::fs::read(&output).expect("read authenticated redirect output"),
        b"auth-redirect\n"
    );
    server.await.expect("auth redirect fixture should finish");
}

#[tokio::test]
async fn conditional_get_304_completes_without_location() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind conditional GET fixture");
    let address = listener.local_addr().expect("read conditional GET address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept conditional GET");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read conditional GET request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /cached.bin HTTP/1.1\r\n"));
        assert!(
            request
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("if-modified-since:")),
            "conditional GET must send If-Modified-Since: {request}"
        );
        stream
            .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
            .await
            .expect("write 304 response");
    });

    let directory = tempfile::tempdir().expect("create conditional GET directory");
    let output = directory.path().join("cached.bin");
    std::fs::write(&output, b"cached bytes").expect("create cached output");
    let options = DownloadOptions {
        allow_overwrite: true,
        conditional_get: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/cached.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(13),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("cached.bin"),
    )
    .expect("create conditional GET command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("conditional GET should not hang")
        .expect("304 should complete the cached download");
    assert_eq!(
        std::fs::read(&output).expect("read cached output"),
        b"cached bytes"
    );

    server.await.expect("conditional GET fixture should finish");
}

#[tokio::test]
async fn unconditional_304_is_rejected_as_http_protocol_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unconditional 304 fixture");
    let address = listener
        .local_addr()
        .expect("read unconditional 304 address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept unconditional 304");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read unconditional 304 request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /cached.bin HTTP/1.1\r\n"));
        assert!(!request.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.starts_with("if-modified-since:") || lower.starts_with("if-none-match:")
        }));
        stream
            .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
            .await
            .expect("write unconditional 304 response");
    });

    let directory = tempfile::tempdir().expect("create unconditional 304 directory");
    let output = directory.path().join("cached.bin");
    std::fs::write(&output, b"cached bytes").expect("create cached output");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        max_retries: 1,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/cached.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(14),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("cached.bin"),
    )
    .expect("create unconditional 304 command");

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("unconditional 304 should not hang")
        .expect_err("unconditional 304 must be rejected");
    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
            if message.contains("304")
    ));
    assert_eq!(
        std::fs::read(&output).expect("read cached output"),
        b"cached bytes"
    );

    server
        .await
        .expect("unconditional 304 fixture should finish");
}
