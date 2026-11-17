use super::*;

#[test]
fn test_packet_type_conversion() {
    assert_eq!(PacketType::from_u8(0), Some(PacketType::StData));
    assert_eq!(PacketType::from_u8(1), Some(PacketType::StFin));
    assert_eq!(PacketType::from_u8(2), Some(PacketType::StAck));
    assert_eq!(PacketType::from_u8(3), Some(PacketType::StReset));
    assert_eq!(PacketType::from_u8(4), Some(PacketType::StSyn));
    assert_eq!(PacketType::from_u8(5), None);

    assert_eq!(PacketType::StData.to_u8(), 0);
    assert_eq!(PacketType::StFin.to_u8(), 1);
    assert_eq!(PacketType::StAck.to_u8(), 2);
    assert_eq!(PacketType::StReset.to_u8(), 3);
    assert_eq!(PacketType::StSyn.to_u8(), 4);
}

#[test]
fn test_packet_type_has_payload() {
    assert!(PacketType::StSyn.has_payload());
    assert!(PacketType::StData.has_payload());
    assert!(!PacketType::StAck.has_payload());
    assert!(!PacketType::StFin.has_payload());
    assert!(!PacketType::StReset.has_payload());
}

#[test]
fn test_syn_packet_creation() {
    let packet = UtpPacket::syn(12345, 1, 0, 0);
    assert_eq!(packet.packet_type().unwrap(), PacketType::StSyn);
    assert_eq!(packet.connection_id, 12345);
    assert_eq!(packet.seq_nr, 1);
    assert_eq!(packet.version(), UTP_VERSION);
}

#[test]
fn test_data_packet_creation() {
    let payload = vec![1, 2, 3, 4, 5];
    let packet = UtpPacket::data(100, 10, 5, 0, payload.clone());
    assert_eq!(packet.packet_type().unwrap(), PacketType::StData);
    assert_eq!(packet.connection_id, 100);
    assert_eq!(packet.seq_nr, 10);
    assert_eq!(packet.ack_nr, 5);
    assert_eq!(packet.payload, payload);
}

#[test]
fn timestamp_difference_uses_wrapping_local_minus_remote_time() {
    let mut packet = UtpPacket::data(1, 2, 0, 1024, vec![1]);
    packet.timestamp_microseconds = u32::MAX - 4;
    packet.timestamp_difference_microseconds = 123_456;

    assert_eq!(packet.calculate_timestamp_diff(7), 12);
}

#[test]
fn test_ack_packet_creation() {
    let packet = UtpPacket::ack(200, 15, 20, 1024);
    assert_eq!(packet.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(packet.connection_id, 200);
    assert_eq!(packet.ack_nr, 15);
    assert_eq!(packet.seq_nr, 20);
    assert_eq!(packet.wnd_size, 1024);
}

#[test]
fn test_fin_packet_creation() {
    let packet = UtpPacket::fin(300, 100, 99, 0);
    assert_eq!(packet.packet_type().unwrap(), PacketType::StFin);
    assert_eq!(packet.connection_id, 300);
    assert_eq!(packet.seq_nr, 100);
    assert_eq!(packet.ack_nr, 99);
}

#[test]
fn test_reset_packet_creation() {
    let packet = UtpPacket::reset(400);
    assert_eq!(packet.packet_type().unwrap(), PacketType::StReset);
    assert_eq!(packet.connection_id, 400);
}

#[test]
fn test_packet_serialization_deserialization() {
    let original = UtpPacket {
        type_ver: (PacketType::StData.to_u8() << 4) | UTP_VERSION,
        extension: 0,
        connection_id: 0x1234,
        timestamp_microseconds: 0xDEADBEEF,
        timestamp_difference_microseconds: 0xCAFEBABE,
        wnd_size: 0x10000,
        seq_nr: 0x5678,
        ack_nr: 0x9ABC,
        payload: vec![1, 2, 3, 4, 5],
    };

    let bytes = original.to_bytes();
    let decoded = UtpPacket::from_bytes(&bytes).unwrap();

    assert_eq!(decoded.type_ver, original.type_ver);
    assert_eq!(decoded.extension, original.extension);
    assert_eq!(decoded.connection_id, original.connection_id);
    assert_eq!(
        decoded.timestamp_microseconds,
        original.timestamp_microseconds
    );
    assert_eq!(
        decoded.timestamp_difference_microseconds,
        original.timestamp_difference_microseconds
    );
    assert_eq!(decoded.wnd_size, original.wnd_size);
    assert_eq!(decoded.seq_nr, original.seq_nr);
    assert_eq!(decoded.ack_nr, original.ack_nr);
    assert_eq!(decoded.payload, original.payload);
}

#[test]
fn test_packet_header_size() {
    let packet = UtpPacket::syn(1, 1, 0, 0);
    let bytes = packet.to_bytes();
    assert_eq!(bytes.len(), UTP_HEADER_SIZE);
}

#[test]
fn test_packet_with_payload_size() {
    let payload = vec![0u8; 100];
    let packet = UtpPacket::data(1, 1, 1, 0, payload);
    let bytes = packet.to_bytes();
    assert_eq!(bytes.len(), UTP_HEADER_SIZE + 100);
}

#[test]
fn test_buffer_too_small_error() {
    let data = [0u8; 10];
    let result = UtpPacket::from_bytes(&data);
    assert!(matches!(
        result,
        Err(UtpPacketError::BufferTooSmall {
            expected: UTP_HEADER_SIZE,
            actual: 10
        })
    ));
}

#[test]
fn test_invalid_version_error() {
    // Create packet with invalid version
    let mut data = vec![0u8; UTP_HEADER_SIZE];
    data[0] = (PacketType::StData.to_u8() << 4) | 2; // version 2 is invalid

    let result = UtpPacket::from_bytes(&data);
    assert!(matches!(
        result,
        Err(UtpPacketError::InvalidVersion {
            expected: UTP_VERSION,
            actual: 2
        })
    ));
}

#[test]
fn test_set_packet_type() {
    let mut packet = UtpPacket::new(PacketType::StSyn);
    assert_eq!(packet.packet_type().unwrap(), PacketType::StSyn);

    packet.set_packet_type(PacketType::StData);
    assert_eq!(packet.packet_type().unwrap(), PacketType::StData);
    assert_eq!(packet.version(), UTP_VERSION); // Version should remain unchanged
}

#[test]
fn test_set_version() {
    let mut packet = UtpPacket::new(PacketType::StSyn);
    packet.set_version(UTP_VERSION);
    assert_eq!(packet.version(), UTP_VERSION);
}

#[test]
fn test_is_valid_version() {
    let packet = UtpPacket::new(PacketType::StSyn);
    assert!(packet.is_valid_version());
}

#[test]
fn test_packet_display() {
    let packet = UtpPacket::syn(12345, 1, 0, 0);
    let display = format!("{}", packet);
    assert!(display.contains("SYN"));
    assert!(display.contains("12345"));
}

#[test]
fn test_empty_payload_packet() {
    let packet = UtpPacket::ack(1, 1, 1, 1024);
    assert!(packet.payload.is_empty());
    let decoded = UtpPacket::from_bytes(&packet.to_bytes()).unwrap();
    assert!(decoded.payload.is_empty());
}

#[test]
fn test_large_payload_packet() {
    let payload = vec![0xAB; 1400]; // Typical MTU-sized payload
    let packet = UtpPacket::data(1, 1, 1, 0, payload.clone());
    let bytes = packet.to_bytes();
    let decoded = UtpPacket::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.payload, payload);
}

#[test]
fn test_packet_type_display() {
    assert_eq!(format!("{}", PacketType::StSyn), "SYN");
    assert_eq!(format!("{}", PacketType::StData), "DATA");
    assert_eq!(format!("{}", PacketType::StAck), "ACK");
    assert_eq!(format!("{}", PacketType::StFin), "FIN");
    assert_eq!(format!("{}", PacketType::StReset), "RESET");
}

#[test]
fn extension_chain_skips_unknown_extensions_and_separates_application_payload() {
    let mut packet = UtpPacket::data(1, 2, 0, 1024, vec![]);
    packet.extension = 42;
    packet.payload = [1, 1, 0xAA, 0, 4, 0x05, 0, 0, 0, b'o', b'k'].to_vec();

    let extensions = packet.parse_extensions().unwrap();
    assert_eq!(extensions.selective_ack, Some([0x05, 0, 0, 0].as_slice()));
    assert_eq!(extensions.application_payload, b"ok");
}

#[test]
fn selective_ack_extension_requires_a_nonempty_32_bit_multiple() {
    let mut packet = UtpPacket::ack(1, 0, 1, 1024);
    packet.extension = SELECTIVE_ACK_EXTENSION;
    packet.payload = [0, 3, 0x01, 0, 0].to_vec();

    assert!(matches!(
        packet.parse_extensions(),
        Err(UtpPacketError::InvalidExtension(SELECTIVE_ACK_EXTENSION))
    ));
}
