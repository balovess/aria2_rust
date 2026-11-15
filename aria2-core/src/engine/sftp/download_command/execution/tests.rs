#[cfg(test)]
mod connect_tests {
    use super::super::*;
    use crate::dns::dns_cache::DnsCache;
    use std::time::Duration;

    #[tokio::test]
    async fn async_dns_sftp_connection_uses_the_shared_cache() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local SSH endpoint");
        let port = listener.local_addr().expect("read endpoint").port();
        let accept = tokio::spawn(async move { listener.accept().await });
        let dns_cache = tokio::sync::Mutex::new(DnsCache::new());

        let stream = connect_sftp_host(
            "localhost",
            port,
            Some(&dns_cache),
            &crate::network::OutboundNetworkPolicy::direct(),
        )
        .await
        .expect("shared DNS cache should resolve localhost and connect");

        assert_eq!(dns_cache.lock().await.len(), 1);
        drop(stream);
        accept
            .await
            .expect("accept task should finish")
            .expect("accept local connection");
    }

    #[tokio::test]
    async fn explicit_dns_server_routes_sftp_hostname_to_resolved_tcp_peer() {
        use tokio::net::UdpSocket;

        let dns_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind explicit DNS fixture");
        let dns_address = dns_socket
            .local_addr()
            .expect("read explicit DNS fixture address");
        let dns_server = tokio::spawn(async move {
            let mut request = [0u8; 512];
            for _ in 0..4 {
                let (length, peer) = dns_socket
                    .recv_from(&mut request)
                    .await
                    .expect("receive explicit DNS query");
                assert!(length >= 17, "DNS query must contain its question");
                let mut question_end = 12;
                loop {
                    let label_length = request[question_end] as usize;
                    question_end += 1;
                    if label_length == 0 {
                        question_end += 4;
                        break;
                    }
                    question_end += label_length;
                }
                let qtype =
                    u16::from_be_bytes([request[question_end - 4], request[question_end - 3]]);
                let has_address = qtype == 1;
                let mut response = Vec::with_capacity(question_end + 16);
                response.extend_from_slice(&request[..2]);
                response.extend_from_slice(&[
                    0x81,
                    0x80,
                    0,
                    1,
                    0,
                    u8::from(has_address),
                    0,
                    0,
                    0,
                    0,
                ]);
                response.extend_from_slice(&request[12..question_end]);
                if has_address {
                    response.extend_from_slice(&[
                        0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1,
                    ]);
                }
                dns_socket
                    .send_to(&response, peer)
                    .await
                    .expect("send explicit DNS response");
                if has_address {
                    break;
                }
            }
        });

        let ssh_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local SSH endpoint");
        let ssh_address = ssh_listener.local_addr().expect("read SSH endpoint");
        let accept = tokio::spawn(async move {
            ssh_listener
                .accept()
                .await
                .expect("accept SFTP TCP connection")
                .1
        });
        let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
            Duration::from_secs(2),
            vec![dns_address],
        ));

        let stream = tokio::time::timeout(
            Duration::from_secs(3),
            connect_sftp_host(
                "sftp.explicit-dns.test",
                ssh_address.port(),
                Some(&dns_cache),
                &crate::network::OutboundNetworkPolicy::direct(),
            ),
        )
        .await
        .expect("SFTP DNS-to-TCP connection should finish promptly")
        .expect("configured DNS answer should connect to the local SSH endpoint");

        assert_eq!(
            stream.peer_addr().expect("read connected peer"),
            ssh_address
        );
        assert_eq!(dns_cache.lock().await.len(), 1);
        drop(stream);
        assert_eq!(
            accept.await.expect("SSH accept task should finish").ip(),
            ssh_address.ip()
        );
        tokio::time::timeout(Duration::from_secs(2), dns_server)
            .await
            .expect("DNS fixture should finish promptly")
            .expect("DNS fixture task should not fail");
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use aria2_protocol::sftp::connection::SshError;
    use std::time::Duration;

    #[tokio::test]
    async fn connect_timeout_covers_a_pending_connection_and_keeps_target_context() {
        let result = connect_with_timeout(
            "sftp.example",
            2222,
            Duration::from_millis(1),
            std::future::pending::<std::result::Result<(), SshError>>(),
        )
        .await;

        assert!(matches!(
            result,
            Err(SshError::ConnectTimeout {
                host,
                port: 2222,
                timeout_secs: 0,
            }) if host == "sftp.example"
        ));
    }

    #[tokio::test]
    async fn lifecycle_removal_interrupts_pending_sftp_setup() {
        let command = SftpDownloadCommand::new(
            GroupId::new(1),
            "sftp://user:password@example.com/file.bin",
            &DownloadOptions::default(),
            None,
            None,
        )
        .expect("construct SFTP command");
        let group = Arc::clone(&command.group);
        let removal = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            group
                .write()
                .unwrap()
                .remove()
                .expect("remove pending SFTP task");
        });

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            command.await_with_lifecycle(std::future::pending::<Result<()>>()),
        )
        .await
        .expect("lifecycle removal should interrupt setup promptly");

        assert!(matches!(result, Err(Aria2Error::DownloadFailed(_))));
        removal.await.expect("removal task should finish");
    }
}
