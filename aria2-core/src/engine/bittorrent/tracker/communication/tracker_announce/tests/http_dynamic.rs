use super::*;

#[tokio::test]
async fn local_http_tracker_returns_dynamic_announce_list_and_uses_new_tier() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local HTTP tracker fixture");
    let address = listener.local_addr().expect("local tracker address");
    let dynamic_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind dynamically announced tracker fixture");
    let dynamic_address = dynamic_listener
        .local_addr()
        .expect("dynamic tracker address");
    let dynamic_a = format!("http://{dynamic_address}/announce");
    let dynamic_b = "http://127.0.0.1:1/dynamic-b".to_string();
    let response_body = {
        let mut response = BTreeMap::new();
        response.insert(b"complete".to_vec(), BencodeValue::Int(7));
        response.insert(b"incomplete".to_vec(), BencodeValue::Int(3));
        response.insert(b"interval".to_vec(), BencodeValue::Int(60));
        response.insert(
            b"announce-list".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::List(vec![BencodeValue::Bytes(dynamic_a.clone().into_bytes())]),
                BencodeValue::List(vec![BencodeValue::Bytes(dynamic_b.clone().into_bytes())]),
            ]),
        );
        response.insert(
            b"peers".to_vec(),
            BencodeValue::Bytes(vec![192, 0, 2, 10, 0x1A, 0xE1]),
        );
        BencodeValue::Dict(response).encode()
    };
    let dynamic_response_body = {
        let mut response = BTreeMap::new();
        response.insert(b"complete".to_vec(), BencodeValue::Int(2));
        response.insert(b"incomplete".to_vec(), BencodeValue::Int(4));
        response.insert(b"interval".to_vec(), BencodeValue::Int(60));
        response.insert(b"peers".to_vec(), BencodeValue::Bytes(Vec::new()));
        BencodeValue::Dict(response).encode()
    };
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept tracker request");
            let mut request = vec![0u8; 4096];
            let _ = socket
                .read(&mut request)
                .await
                .expect("read tracker request");
            if request_index == 1 {
                socket
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("reject the second primary tracker announce");
                continue;
            }
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write tracker headers");
            socket
                .write_all(&response_body)
                .await
                .expect("write tracker response");
        }
    });
    let dynamic_server = tokio::spawn(async move {
        let (mut socket, _) = dynamic_listener
            .accept()
            .await
            .expect("accept dynamically announced tracker request");
        let mut request = vec![0u8; 4096];
        let _ = socket
            .read(&mut request)
            .await
            .expect("read dynamic tracker request");
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
            dynamic_response_body.len()
        );
        socket
            .write_all(headers.as_bytes())
            .await
            .expect("write dynamic tracker headers");
        socket
            .write_all(&dynamic_response_body)
            .await
            .expect("write dynamic tracker response");
    });

    let initial = format!("http://{address}/announce");
    let mut announcer = TrackerAnnouncer::new(&[vec![initial.clone()]], &None);
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));

    let result = announcer
        .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
        .await
        .expect("HTTP tracker fixture should return an announce result");
    assert_eq!(result.peers, vec![("192.0.2.10".to_string(), 6881)]);
    assert_eq!(result.seeders, Some(7));
    assert_eq!(result.leechers, Some(3));
    assert!(announcer.announce.announce_list().contains_url(&initial));
    assert!(announcer.announce.announce_list().contains_url(&dynamic_a));
    assert!(announcer.announce.announce_list().contains_url(&dynamic_b));
    assert_eq!(announcer.announce.announce_list().tier_count(), 3);

    // Skip the tracker-supplied minimum interval without sleeping, so the
    // test covers the actual primary-failure to appended-tier transition.
    announcer.announce.override_min_interval(Duration::ZERO);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(4),
            announcer.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
        )
        .await
        .expect("primary failure response should arrive")
        .is_none(),
        "the second primary announce is intentionally rejected"
    );
    let dynamic_result = tokio::time::timeout(
        Duration::from_secs(4),
        announcer.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
    )
    .await
    .expect("dynamic tracker announce should finish")
    .expect("the newly advertised tracker should succeed");
    assert_eq!(dynamic_result.tracker_url, dynamic_a);
    assert_eq!(dynamic_result.seeders, Some(2));
    assert_eq!(dynamic_result.leechers, Some(4));

    server.await.expect("HTTP tracker fixture should exit");
    dynamic_server
        .await
        .expect("dynamic tracker fixture should exit");
}
