use super::*;

#[cfg(test)]
mod http_status_tests {
    use super::*;
    use crate::request::request_group::DownloadOptions;

    #[test]
    fn classifies_5xx_as_retryable_server_errors() {
        assert!(matches!(
            classify_metalink_http_status(503),
            Aria2Error::Recoverable(RecoverableError::ServerError { code: 503 })
        ));
    }

    #[test]
    fn classifies_configured_4xx_transients_as_retryable_server_errors() {
        for status_code in [408, 429] {
            assert!(matches!(
                classify_metalink_http_status(status_code),
                Aria2Error::Recoverable(RecoverableError::ServerError { code })
                    if code == status_code
            ));
        }
    }

    #[test]
    fn classifies_not_found_as_resource_not_found() {
        assert!(matches!(
            classify_metalink_http_status(404),
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
        ));
    }

    #[test]
    fn classifies_authentication_statuses_as_http_auth_failures() {
        for status_code in [401, 407] {
            assert!(matches!(
                classify_metalink_http_status(status_code),
                Aria2Error::Recoverable(RecoverableError::HttpAuthFailed { message })
                    if message == format!("authentication failed: HTTP {status_code}")
            ));
        }
    }

    #[tokio::test]
    async fn mirror_not_found_respects_request_group_limit() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("404 fixture should bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_count = Arc::clone(&request_count);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("404 request");
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request).await.expect("read 404 request");
                server_count.fetch_add(1, Ordering::SeqCst);
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write 404 response");
            }
        });

        let output_dir = tempfile::tempdir().expect("output directory");
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="payload.bin">
    <size>1</size>
    <url>{base_url}/first</url>
    <url>{base_url}/second</url>
  </file>
</metalink>"#
        );
        let options = DownloadOptions {
            dir: Some(output_dir.path().to_string_lossy().into_owned()),
            ..DownloadOptions::default()
        };
        let mut command =
            MetalinkDownloadCommand::new(GroupId::new(401), xml.as_bytes(), &options, None)
                .expect("Metalink command should construct");
        command
            .group
            .recover_mut()
            .set_option_snapshot(std::collections::HashMap::from([(
                "max-file-not-found".to_string(),
                serde_json::json!("2"),
            )]));

        let error = command
            .execute()
            .await
            .expect_err("second 404 must stop the group");
        assert!(matches!(
            error,
            Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
        ));
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        server.await.expect("404 fixture should finish");
    }

    #[tokio::test]
    async fn mirror_not_found_zero_does_not_fail_over_to_next_mirror() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("404 fixture should bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_count = Arc::clone(&request_count);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("first 404 request");
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.expect("read first request");
            server_count.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write first 404 response");

            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
            {
                let _ = stream
                    .read(&mut request)
                    .await
                    .expect("read second request");
                server_count.fetch_add(1, Ordering::SeqCst);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
                    )
                    .await
                    .expect("write second response");
            }
        });

        let output_dir = tempfile::tempdir().expect("output directory");
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="payload.bin">
    <size>1</size>
    <url>{base_url}/first</url>
    <url>{base_url}/second</url>
  </file>
</metalink>"#
        );
        let options = DownloadOptions {
            dir: Some(output_dir.path().to_string_lossy().into_owned()),
            ..DownloadOptions::default()
        };
        let mut command =
            MetalinkDownloadCommand::new(GroupId::new(402), xml.as_bytes(), &options, None)
                .expect("Metalink command should construct");
        command
            .group
            .recover_mut()
            .set_option_snapshot(std::collections::HashMap::from([(
                "max-file-not-found".to_string(),
                serde_json::json!("0"),
            )]));

        let output_path = command.output_path.clone();
        let error = command
            .execute()
            .await
            .expect_err("max-file-not-found=0 must stop after the first 404");
        assert!(matches!(
            error,
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
        ));
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        assert!(!output_path.exists());
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("404 fixture should finish")
            .expect("404 fixture task should succeed");
    }

    #[tokio::test]
    async fn mirror_504_retries_before_completing() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("504 fixture should bind");
        let url = format!("http://{}/payload", listener.local_addr().unwrap());
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_count = Arc::clone(&request_count);
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("mirror request");
                let mut request = [0u8; 2048];
                let _ = stream
                    .read(&mut request)
                    .await
                    .expect("read mirror request");
                server_count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write 504 response");
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
                        )
                        .await
                        .expect("write success response");
                }
            }
        });

        let output_dir = tempfile::tempdir().expect("output directory");
        let xml = format!(
            r#"<?xml version="1.0"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="payload.bin">
    <size>1</size>
    <url>{url}</url>
  </file>
</metalink>"#
        );
        let options = DownloadOptions {
            dir: Some(output_dir.path().to_string_lossy().into_owned()),
            max_retries: 2,
            retry_wait: 0,
            ..DownloadOptions::default()
        };
        let mut command =
            MetalinkDownloadCommand::new(GroupId::new(403), xml.as_bytes(), &options, None)
                .expect("Metalink command should construct");

        command.execute().await.expect("504 retry should complete");

        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert_eq!(tokio::fs::read(command.output_path()).await.unwrap(), b"x");
        server.await.expect("504 fixture should finish");
    }

    #[cfg(feature = "bittorrent")]
    #[tokio::test]
    async fn torrent_metaurl_504_retries_before_returning_metadata() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("torrent metaurl fixture should bind");
        let url = format!("http://{}/payload.torrent", listener.local_addr().unwrap());
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_count = Arc::clone(&request_count);
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("metadata request");
                let mut request = [0u8; 2048];
                let _ = stream
                    .read(&mut request)
                    .await
                    .expect("read metadata request");
                server_count.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write 504 response");
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
                        )
                        .await
                        .expect("write metadata response");
                }
            }
        });

        let options = DownloadOptions {
            max_retries: 2,
            retry_wait: 0,
            ..DownloadOptions::default()
        };
        let command = MetalinkDownloadCommand::new(
            GroupId::new(404),
            br#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>http://127.0.0.1/unused</url></file></metalink>"#,
            &options,
            None,
        )
        .expect("Metalink command should construct");

        let metadata = command
            .download_metadata_url_with_retry(&url)
            .await
            .expect("504 retry should return metadata");

        assert_eq!(metadata, b"x");
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        server.await.expect("torrent metaurl fixture should finish");
    }

    #[cfg(feature = "bittorrent")]
    #[tokio::test]
    async fn slow_torrent_metadata_read_observes_pause() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::oneshot;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("metadata fixture should bind");
        let url = format!("http://{}/payload.torrent", listener.local_addr().unwrap());
        let (headers_tx, headers_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("metadata request");
            let mut request = [0u8; 2048];
            let _ = stream
                .read(&mut request)
                .await
                .expect("read metadata request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
                .await
                .expect("write metadata headers");
            let _ = headers_tx.send(());
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let options = DownloadOptions::default();
        let command = MetalinkDownloadCommand::new(
            GroupId::new(402),
            br#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><size>4</size><url>http://127.0.0.1/unused</url></file></metalink>"#,
            &options,
            None,
        )
        .expect("Metalink command should construct");
        let group = Arc::clone(&command.group);
        let command_task = tokio::spawn(async move { command.download_metadata_url(&url).await });

        headers_rx.await.expect("metadata headers should be sent");
        group.recover_mut().pause().expect("pause should succeed");

        let result = tokio::time::timeout(Duration::from_secs(1), command_task)
            .await
            .expect("metadata read should stop promptly after pause")
            .expect("metadata task should not panic")
            .expect_err("paused metadata read must return an error");
        assert!(matches!(
            result,
            Aria2Error::DownloadFailed(message) if message == "Download paused"
        ));
        server.abort();
    }
}
