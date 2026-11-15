use super::*;

#[tokio::test]
async fn ftp_proxy_records_each_payload_chunk_for_timeout() {
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
            assert!(bytes_read > 0, "FTP proxy fixture should receive a request");
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

    let output_dir = tempfile::tempdir().unwrap();
    let url = "ftp://ftp.example.test/pub/proxy-timeout.bin".to_string();
    let options = DownloadOptions {
        ftp_proxy: Some(format!("http://{address}")),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9002),
        vec![url.clone()],
        options,
    )));
    let mut command =
        FtpDownloadCommand::new_with_group(Arc::clone(&group), output_dir.path().to_str(), None)
            .unwrap();

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
    .expect("FTP proxy did not receive its first payload chunk");
    let first_activity = group.recover().last_network_activity();

    tokio::time::timeout(Duration::from_secs(5), command_task)
        .await
        .expect("FTP proxy command did not complete")
        .expect("FTP proxy command panicked")
        .expect("FTP proxy command failed");
    server.await.unwrap();

    assert!(
        group.recover().last_network_activity() > first_activity,
        "each non-empty FTP proxy payload chunk must refresh the inactivity clock"
    );
}

#[tokio::test]
async fn ftp_reuses_authenticated_control_session_across_downloads() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_address = control_listener.local_addr().unwrap();
    let _proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = _proxy_listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = control_listener.accept().await.unwrap();
        let mut control = BufReader::new(stream);
        control
            .get_mut()
            .write_all(b"220 local FTP fixture\r\n")
            .await
            .unwrap();
        let mut data_listener = None;
        let mut transfer_count = 0;
        let mut login_count = 0;
        let mut command = String::new();
        while control.read_line(&mut command).await.unwrap() != 0 {
            let line = command.trim_end_matches(['\r', '\n']).to_owned();
            command.clear();
            match line
                .split_once(' ')
                .map_or((line.as_str(), ""), |(verb, arg)| (verb, arg))
            {
                ("USER", _) => {
                    login_count += 1;
                    control
                        .get_mut()
                        .write_all(b"331 password required\r\n")
                        .await
                        .unwrap();
                }
                ("PASS", _) => control
                    .get_mut()
                    .write_all(b"230 logged in\r\n")
                    .await
                    .unwrap(),
                ("TYPE", _) => control
                    .get_mut()
                    .write_all(b"200 type set\r\n")
                    .await
                    .unwrap(),
                ("PWD", _) => control.get_mut().write_all(b"257 \"/\"\r\n").await.unwrap(),
                ("CWD", _) => control
                    .get_mut()
                    .write_all(b"250 directory changed\r\n")
                    .await
                    .unwrap(),
                ("SIZE", _) => control.get_mut().write_all(b"213 4\r\n").await.unwrap(),
                ("REST", _) => control
                    .get_mut()
                    .write_all(b"350 restart accepted\r\n")
                    .await
                    .unwrap(),
                ("EPSV", _) => {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = listener.local_addr().unwrap().port();
                    data_listener = Some(listener);
                    control
                        .get_mut()
                        .write_all(
                            format!("229 Entering Extended Passive Mode (|||{port}|)\r\n")
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
                ("PASV", _) => {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = listener.local_addr().unwrap().port();
                    data_listener = Some(listener);
                    control
                        .get_mut()
                        .write_all(
                            format!(
                                "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                                port / 256,
                                port % 256
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
                ("RETR", _) => {
                    control
                        .get_mut()
                        .write_all(b"150 opening data connection\r\n")
                        .await
                        .unwrap();
                    let listener = data_listener.take().expect("EPSV must precede RETR");
                    let (mut data, _) = listener.accept().await.unwrap();
                    data.write_all(b"data").await.unwrap();
                    data.shutdown().await.unwrap();
                    control
                        .get_mut()
                        .write_all(b"226 transfer complete\r\n")
                        .await
                        .unwrap();
                    transfer_count += 1;
                    if transfer_count == 2 {
                        break;
                    }
                }
                other => panic!("unexpected FTP command: {other:?}"),
            }
        }
        (login_count, transfer_count)
    });

    let pool = Arc::new(crate::ftp::connection_pool::FtpConnectionPool::new(4));
    let output_dir = tempfile::tempdir().unwrap();
    for (index, filename) in ["first.bin", "second.bin"].into_iter().enumerate() {
        let uri = format!("ftp://reuse-user:reuse-pass@{control_address}/{filename}");
        let options = DownloadOptions {
            allow_overwrite: true,
            ftp_proxy: Some(format!("http://{proxy_address}")),
            no_proxy: Some("127.0.0.1".to_string()),
            ..DownloadOptions::default()
        };
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(9100 + index as u64),
            vec![uri],
            options,
        )));
        let mut command = FtpDownloadCommand::new_with_group(
            Arc::clone(&group),
            output_dir.path().to_str(),
            None,
        )
        .unwrap();
        command.set_connection_pool(Arc::clone(&pool));
        tokio::time::timeout(Duration::from_secs(5), command.execute())
            .await
            .expect("FTP download should finish")
            .expect("FTP download should succeed");
        assert_eq!(
            std::fs::read(output_dir.path().join(filename)).unwrap(),
            b"data"
        );
        assert_eq!(pool.size().await, 1);
    }

    let (login_count, transfer_count) = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("FTP fixture should finish")
        .expect("FTP fixture should not panic");
    assert_eq!(login_count, 1, "second download should reuse the login");
    assert_eq!(transfer_count, 2);
    let stats = pool.stats().await;
    assert_eq!(stats.connections_created, 1);
    assert_eq!(stats.connections_reused, 1);
}

#[tokio::test]
async fn ftp_reconnects_after_a_pooled_control_socket_goes_stale() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let control_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_address = control_listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut login_count = 0;
        for session_index in 0..2 {
            let (stream, _) =
                tokio::time::timeout(Duration::from_secs(5), control_listener.accept())
                    .await
                    .expect("FTP client should connect or recover from the stale session")
                    .unwrap();
            let mut control = BufReader::new(stream);
            control
                .get_mut()
                .write_all(b"220 local FTP fixture\r\n")
                .await
                .unwrap();
            let mut data_listener = None;
            let mut command = String::new();
            loop {
                if control.read_line(&mut command).await.unwrap() == 0 {
                    break;
                }
                let line = command.trim_end_matches(['\r', '\n']).to_owned();
                command.clear();
                match line
                    .split_once(' ')
                    .map_or((line.as_str(), ""), |(verb, arg)| (verb, arg))
                {
                    ("USER", _) => {
                        login_count += 1;
                        control
                            .get_mut()
                            .write_all(b"331 password required\r\n")
                            .await
                            .unwrap();
                    }
                    ("PASS", _) => {
                        control
                            .get_mut()
                            .write_all(b"230 logged in\r\n")
                            .await
                            .unwrap();
                    }
                    ("TYPE", _) => {
                        control
                            .get_mut()
                            .write_all(b"200 type set\r\n")
                            .await
                            .unwrap();
                    }
                    ("PWD", _) => {
                        control.get_mut().write_all(b"257 \"/\"\r\n").await.unwrap();
                    }
                    ("CWD", _) => {
                        control
                            .get_mut()
                            .write_all(b"250 directory changed\r\n")
                            .await
                            .unwrap();
                    }
                    ("SIZE", _) => {
                        control.get_mut().write_all(b"213 4\r\n").await.unwrap();
                    }
                    ("REST", _) => {
                        control
                            .get_mut()
                            .write_all(b"350 restart accepted\r\n")
                            .await
                            .unwrap();
                    }
                    ("EPSV", _) => {
                        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let port = listener.local_addr().unwrap().port();
                        data_listener = Some(listener);
                        control
                            .get_mut()
                            .write_all(
                                format!("229 Entering Extended Passive Mode (|||{port}|)\r\n")
                                    .as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                    ("PASV", _) => {
                        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let port = listener.local_addr().unwrap().port();
                        data_listener = Some(listener);
                        control
                            .get_mut()
                            .write_all(
                                format!(
                                    "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                                    port / 256,
                                    port % 256
                                )
                                .as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                    ("RETR", _) => {
                        control
                            .get_mut()
                            .write_all(b"150 opening data connection\r\n")
                            .await
                            .unwrap();
                        let listener = data_listener.take().expect("EPSV must precede RETR");
                        let (mut data, _) = listener.accept().await.unwrap();
                        data.write_all(b"data").await.unwrap();
                        data.shutdown().await.unwrap();
                        control
                            .get_mut()
                            .write_all(b"226 transfer complete\r\n")
                            .await
                            .unwrap();
                        break;
                    }
                    other => panic!("unexpected FTP command: {other:?}"),
                }
            }
            // The first successful session is intentionally closed while idle.
            // The next task initially leases this stale entry, then must retry
            // over a fresh authenticated connection after its control read fails.
            drop(control);
            if session_index == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        login_count
    });

    let pool = Arc::new(crate::ftp::connection_pool::FtpConnectionPool::new(4));
    let output_dir = tempfile::tempdir().unwrap();
    for (index, filename) in ["first-stale.bin", "second-stale.bin"]
        .into_iter()
        .enumerate()
    {
        let uri = format!("ftp://stale-user:stale-pass@{control_address}/{filename}");
        let options = DownloadOptions {
            allow_overwrite: true,
            max_retries: 2,
            retry_wait: 0,
            ..DownloadOptions::default()
        };
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(9200 + index as u64),
            vec![uri],
            options,
        )));
        let mut command = FtpDownloadCommand::new_with_group(
            Arc::clone(&group),
            output_dir.path().to_str(),
            None,
        )
        .unwrap();
        command.set_connection_pool(Arc::clone(&pool));
        tokio::time::timeout(Duration::from_secs(5), command.execute())
            .await
            .expect("stale FTP session recovery should not hang")
            .expect("FTP retry should recover with a fresh control session");
        assert_eq!(
            std::fs::read(output_dir.path().join(filename)).unwrap(),
            b"data"
        );
    }

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("FTP stale-session fixture should finish")
            .expect("FTP stale-session fixture should not panic"),
        2,
        "the replacement connection should authenticate again"
    );
    assert_eq!(pool.stats().await.connections_reused, 1);
}
