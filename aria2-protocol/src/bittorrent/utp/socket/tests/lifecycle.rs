use super::*;

#[tokio::test]
async fn retransmit_timer_sends_only_its_packet_and_preserves_backoff() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();
    let client_readiness = client.readiness_socket().unwrap();

    let mut wire_buffer = [0; 2048];
    let (length, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let syn = UtpPacket::from_bytes(&wire_buffer[..length]).unwrap();
    peer.send_to(
        &UtpPacket::syn_ack(syn.connection_id, 70, syn.seq_nr, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Established
    );

    assert_eq!(client.send(conn_id, &vec![0x5a; 2800]).unwrap(), 2800);
    let (first_len, _) = peer.recv_from(&mut wire_buffer).await.unwrap();
    let first = UtpPacket::from_bytes(&wire_buffer[..first_len]).unwrap();
    let (second_len, _) = peer.recv_from(&mut wire_buffer).await.unwrap();
    let second = UtpPacket::from_bytes(&wire_buffer[..second_len]).unwrap();
    assert_ne!(first.seq_nr, second.seq_nr);

    client
        .timers
        .set_timer(conn_id, TimerType::Retransmit(first.seq_nr), Duration::ZERO);
    client.process_timers().unwrap();

    let (retry_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let retry = UtpPacket::from_bytes(&wire_buffer[..retry_len]).unwrap();
    assert_eq!(retry.seq_nr, first.seq_nr);
    assert_eq!(retry.payload, first.payload);
    assert_eq!(
        client.timers.retransmit_count(conn_id, first.seq_nr),
        Some(1)
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv_from(&mut wire_buffer))
            .await
            .is_err()
    );
    assert!(
        client
            .timers
            .has_timer(conn_id, TimerType::Retransmit(second.seq_nr))
    );

    peer.send_to(
        &UtpPacket::ack(syn.connection_id, first.seq_nr, 71, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;
    assert!(
        !client
            .timers
            .has_timer(conn_id, TimerType::Retransmit(first.seq_nr))
    );
    assert!(
        client
            .timers
            .has_timer(conn_id, TimerType::Retransmit(second.seq_nr))
    );
}

#[tokio::test]
async fn close_connection_retransmits_fin_until_acknowledged() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();
    let client_readiness = client.readiness_socket().unwrap();

    let mut wire_buffer = [0; 2048];
    let (syn_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let syn = UtpPacket::from_bytes(&wire_buffer[..syn_len]).unwrap();
    peer.send_to(
        &UtpPacket::syn_ack(syn.connection_id, 70, syn.seq_nr, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Established
    );

    client.close_connection(conn_id).unwrap();
    let (fin_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let fin = UtpPacket::from_bytes(&wire_buffer[..fin_len]).unwrap();
    assert_eq!(fin.packet_type().unwrap(), PacketType::StFin);
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::FinWait
    );

    let retry_delay = client
        .next_timer_delay()
        .expect("unacknowledged FIN must retain a retransmission deadline");
    tokio::time::sleep(retry_delay + Duration::from_millis(10)).await;
    client.process_timers().unwrap();

    let (retry_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let retry = UtpPacket::from_bytes(&wire_buffer[..retry_len]).unwrap();
    assert_eq!(retry.packet_type().unwrap(), PacketType::StFin);
    assert_eq!(retry.connection_id, fin.connection_id);
    assert_eq!(retry.seq_nr, fin.seq_nr);

    peer.send_to(
        &UtpPacket::ack(syn.connection_id, fin.seq_nr, 71, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;
    assert!(matches!(
        client.connection_state(conn_id),
        Err(UtpSocketError::ConnectionNotFound(id)) if id == conn_id
    ));
}

#[tokio::test]
async fn received_fin_is_acknowledged_again_and_retired_after_idle_deadline() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    client.set_idle_timeout(Duration::from_millis(40));
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();
    let client_readiness = client.readiness_socket().unwrap();

    let mut wire_buffer = [0; 2048];
    let (syn_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let syn = UtpPacket::from_bytes(&wire_buffer[..syn_len]).unwrap();
    peer.send_to(
        &UtpPacket::syn_ack(syn.connection_id, 70, syn.seq_nr, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;

    let fin = UtpPacket::fin(syn.connection_id, 71, syn.seq_nr, 65_536);
    peer.send_to(&fin.to_bytes(), client.local_addr())
        .await
        .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;
    let (ack_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let ack = UtpPacket::from_bytes(&wire_buffer[..ack_len]).unwrap();
    assert_eq!(ack.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(ack.ack_nr, fin.seq_nr);
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Closing
    );

    peer.send_to(&fin.to_bytes(), client.local_addr())
        .await
        .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;
    let (duplicate_ack_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let duplicate_ack = UtpPacket::from_bytes(&wire_buffer[..duplicate_ack_len]).unwrap();
    assert_eq!(duplicate_ack.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(duplicate_ack.ack_nr, fin.seq_nr);

    let idle_deadline = client
        .next_timer_delay()
        .expect("closing connection must have a bounded retirement deadline");
    tokio::time::sleep(idle_deadline + Duration::from_millis(5)).await;
    client.process_timers().unwrap();
    assert!(matches!(
        client.connection_state(conn_id),
        Err(UtpSocketError::ConnectionNotFound(id)) if id == conn_id
    ));
}

#[tokio::test]
async fn unacknowledged_fin_retires_connection_after_retry_budget() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    client.set_idle_timeout(Duration::from_secs(60));
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();
    let client_readiness = client.readiness_socket().unwrap();

    let mut wire_buffer = [0; 2048];
    let (syn_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let syn = UtpPacket::from_bytes(&wire_buffer[..syn_len]).unwrap();
    peer.send_to(
        &UtpPacket::syn_ack(syn.connection_id, 70, syn.seq_nr, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    poll_recv_after_readable(&mut client, &client_readiness).await;

    client.close_connection(conn_id).unwrap();
    let (fin_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let fin = UtpPacket::from_bytes(&wire_buffer[..fin_len]).unwrap();
    assert_eq!(fin.packet_type().unwrap(), PacketType::StFin);

    for _ in 0..5 {
        let retry_delay = client
            .next_timer_delay()
            .expect("unacknowledged FIN must keep a retry deadline");
        tokio::time::sleep(retry_delay + Duration::from_millis(5)).await;
        client.process_timers().unwrap();

        let (retry_len, _) =
            tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
                .await
                .unwrap()
                .unwrap();
        let retry = UtpPacket::from_bytes(&wire_buffer[..retry_len]).unwrap();
        assert_eq!(retry.packet_type().unwrap(), PacketType::StFin);
        assert_eq!(retry.seq_nr, fin.seq_nr);
    }

    let final_deadline = client
        .next_timer_delay()
        .expect("retry budget should end at a final bounded deadline");
    tokio::time::sleep(final_deadline + Duration::from_millis(5)).await;
    client.process_timers().unwrap();
    assert!(matches!(
        client.connection_state(conn_id),
        Err(UtpSocketError::ConnectionNotFound(id)) if id == conn_id
    ));
}
