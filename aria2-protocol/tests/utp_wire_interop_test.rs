#![cfg(feature = "bittorrent")]

use aria2_protocol::bittorrent::utp::{
    ConnectionState, PacketType, UtpConnection, UtpPacket, UtpSocket,
};
use std::time::Duration;
use tokio::net::UdpSocket;

async fn receive(peer: &UdpSocket) -> UtpPacket {
    let mut bytes = [0; 2048];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut bytes))
        .await
        .expect("wire response deadline")
        .unwrap();
    UtpPacket::from_bytes(&bytes[..length]).unwrap()
}

async fn readable(socket: &UtpSocket) {
    tokio::time::timeout(
        Duration::from_secs(2),
        socket.readiness_socket().unwrap().readable(),
    )
    .await
    .expect("UDP readiness deadline")
    .unwrap();
}

#[tokio::test]
async fn initiator_uses_directional_ids_and_rejects_wrong_endpoint_or_id() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let handle = client.connect(peer.local_addr().unwrap()).unwrap();
    let syn = receive(&peer).await;
    assert_eq!(syn.packet_type().unwrap(), PacketType::StSyn);
    assert_eq!(syn.seq_nr, 1);
    let receive_id = syn.connection_id;
    let send_id = receive_id.wrapping_add(1);
    peer.send_to(
        &UtpPacket::ack(receive_id, syn.seq_nr, 70, 65536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    readable(&client).await;
    client.poll_recv().unwrap();
    assert_eq!(
        client.connection_state(handle).unwrap(),
        ConnectionState::Established
    );
    assert_eq!(client.send(handle, b"ab").unwrap(), 2);
    let data = receive(&peer).await;
    assert_eq!(data.connection_id, send_id);
    assert_eq!(data.seq_nr, 2);

    peer.send_to(
        &UtpPacket::data(send_id, 71, 2, 65536, b"wrong".to_vec()).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    readable(&client).await;
    assert_eq!(client.recv(handle, &mut [0; 16]).unwrap(), 0);
    let intruder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    intruder
        .send_to(
            &UtpPacket::reset(receive_id).to_bytes(),
            client.local_addr(),
        )
        .await
        .unwrap();
    readable(&client).await;
    client.poll_recv().unwrap();
    assert_eq!(
        client.connection_state(handle).unwrap(),
        ConnectionState::Established
    );

    peer.send_to(
        &UtpPacket::data(receive_id, 71, 2, 65536, b"ok".to_vec()).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    readable(&client).await;
    let mut bytes = [0; 16];
    assert_eq!(client.recv(handle, &mut bytes).unwrap(), 2);
    assert_eq!(&bytes[..2], b"ok");
    assert_eq!(receive(&peer).await.connection_id, send_id);
}

#[tokio::test]
async fn acceptor_uses_state_response_and_wrapping_directional_ids() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
    let syn = UtpPacket::syn(u16::MAX, 1, 0, 65536);
    peer.send_to(&syn.to_bytes(), server.local_addr())
        .await
        .unwrap();
    readable(&server).await;
    server.poll_recv().unwrap();
    let state = receive(&peer).await;
    assert_eq!(state.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(state.connection_id, u16::MAX);
    assert_eq!(state.ack_nr, 1);
    let handle = server.connection_ids()[0];
    assert_eq!(
        server.connection_stats(handle).unwrap().local_connection_id,
        0
    );
    peer.send_to(
        &UtpPacket::data(0, 2, state.seq_nr, 65536, b"in".to_vec()).to_bytes(),
        server.local_addr(),
    )
    .await
    .unwrap();
    readable(&server).await;
    let mut bytes = [0; 2];
    assert_eq!(server.recv(handle, &mut bytes).unwrap(), 2);
    assert_eq!(&bytes, b"in");
    assert_eq!(receive(&peer).await.connection_id, u16::MAX);
    assert_eq!(server.send(handle, b"out").unwrap(), 3);
    assert_eq!(receive(&peer).await.connection_id, u16::MAX);
}

#[tokio::test]
async fn receive_orders_wrapping_sequences_deduplicates_and_keeps_ack_directions_separate() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let handle = client.connect(peer.local_addr().unwrap()).unwrap();
    let syn = receive(&peer).await;
    peer.send_to(
        &UtpPacket::ack(handle, syn.seq_nr, 65533, 65536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    readable(&client).await;
    client.poll_recv().unwrap();
    let mut bytes = [0; 16];
    for (sequence, payload, expected, ack) in [
        (65535, b"B".as_slice(), b"".as_slice(), 65533),
        (65534, b"A".as_slice(), b"AB".as_slice(), 65535),
        (0, b"C".as_slice(), b"C".as_slice(), 0),
        (0, b"C".as_slice(), b"".as_slice(), 0),
    ] {
        peer.send_to(
            &UtpPacket::data(handle, sequence, 1, 65536, payload.to_vec()).to_bytes(),
            client.local_addr(),
        )
        .await
        .unwrap();
        readable(&client).await;
        let count = client.recv(handle, &mut bytes).unwrap();
        assert_eq!(&bytes[..count], expected);
        assert_eq!(receive(&peer).await.ack_nr, ack);
    }
    assert_eq!(client.send(handle, b"local").unwrap(), 5);
    let sent = receive(&peer).await;
    peer.send_to(
        &UtpPacket::ack(handle, sent.seq_nr, 123, 65536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    readable(&client).await;
    client.poll_recv().unwrap();
    assert_eq!(client.connection_stats(handle).unwrap().bytes_in_flight, 0);
    peer.send_to(
        &UtpPacket::fin(handle, 2, sent.seq_nr, 65536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    readable(&client).await;
    assert_eq!(client.recv(handle, &mut bytes).unwrap(), 0);
    assert_eq!(
        client.connection_state(handle).unwrap(),
        ConnectionState::Established
    );
    assert_eq!(receive(&peer).await.ack_nr, 0);
    peer.send_to(
        &UtpPacket::data(handle, 1, sent.seq_nr, 65536, b"D".to_vec()).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    readable(&client).await;
    assert_eq!(client.recv(handle, &mut bytes).unwrap(), 1);
    assert_eq!(bytes[0], b'D');
    assert_eq!(
        client.connection_state(handle).unwrap(),
        ConnectionState::Closing
    );
    assert_eq!(receive(&peer).await.ack_nr, 2);
}

#[test]
fn retransmission_keeps_wire_packets_and_acknowledges_exact_payload_bytes() {
    let mut connection = UtpConnection::new();
    let syn = connection
        .connect("127.0.0.1:6881".parse().unwrap())
        .unwrap();
    connection
        .on_packet_received(&UtpPacket::syn_ack(syn.connection_id, 100, 1, 65536))
        .unwrap();
    let packets = connection.send_data(&vec![7; 3000]).unwrap();
    assert_eq!(packets.len(), 2);
    assert_eq!(connection.bytes_in_flight(), 2800);
    for (original, retry) in packets.iter().zip(connection.get_sendable_packets()) {
        assert_eq!(original.seq_nr, retry.seq_nr);
        assert_eq!(original.payload, retry.payload);
        assert_eq!(original.connection_id, retry.connection_id);
    }
    connection
        .on_packet_received(&UtpPacket::ack(syn.connection_id, 2, 101, 65536))
        .unwrap();
    assert_eq!(connection.bytes_in_flight(), 1400);
    assert_eq!(connection.get_sendable_packets().len(), 1);
    assert_eq!(connection.get_sendable_packets()[0].seq_nr, 3);
    connection
        .on_packet_received(&UtpPacket::ack(syn.connection_id, 2, 101, 65536))
        .unwrap();
    connection
        .on_packet_received(&UtpPacket::ack(syn.connection_id, 55, 101, 65536))
        .unwrap();
    assert_eq!(connection.bytes_in_flight(), 1400);
    connection
        .on_packet_received(&UtpPacket::ack(syn.connection_id, 3, 101, 0))
        .unwrap();
    assert_eq!(connection.bytes_in_flight(), 0);
    assert!(connection.get_sendable_packets().is_empty());
    assert!(connection.send_data(b"blocked").unwrap().is_empty());
    assert_eq!(connection.current_ack_nr(), 100);
}

#[tokio::test]
async fn same_endpoint_connections_and_retried_syn_preserve_independent_streams() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
    let mut first_response_sequence = 0;
    for send_id in [7, 9, 7] {
        peer.send_to(
            &UtpPacket::syn(send_id, 1, 0, 65536).to_bytes(),
            server.local_addr(),
        )
        .await
        .unwrap();
        readable(&server).await;
        server.poll_recv().unwrap();
        let response = receive(&peer).await;
        assert_eq!(response.connection_id, send_id);
        if server.connection_count() == 1 {
            first_response_sequence = response.seq_nr;
        } else if send_id == 7 {
            assert_eq!(response.seq_nr, first_response_sequence);
        }
    }
    assert_eq!(server.connection_count(), 2);
    for (receive_id, payload) in [(8, b"one"), (10, b"two")] {
        peer.send_to(
            &UtpPacket::data(receive_id, 2, 0, 65536, payload.to_vec()).to_bytes(),
            server.local_addr(),
        )
        .await
        .unwrap();
        readable(&server).await;
        let mut bytes = [0; 3];
        assert_eq!(server.recv(receive_id, &mut bytes).unwrap(), 3);
        assert_eq!(&bytes, payload);
        assert_eq!(receive(&peer).await.connection_id, receive_id - 1);
    }
}
