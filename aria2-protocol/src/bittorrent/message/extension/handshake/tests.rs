use super::*;

// ======================== ExtensionHandshake tests ========================

#[test]
fn test_handshake_default_creation() {
    let hs = ExtensionHandshake::new();
    assert_eq!(hs.ut_metadata_id(), Some(1));
    assert_eq!(hs.ut_pex_id(), Some(2));
    assert_eq!(hs.reqq(), 500);
    assert_eq!(hs.v(), Some(DEFAULT_CLIENT_VERSION));
    assert_eq!(hs.metadata_size(), None);
    assert_eq!(hs.port(), None);
}

#[test]
fn test_handshake_default_trait() {
    let hs = ExtensionHandshake::default();
    assert_eq!(hs.ut_metadata_id(), Some(1));
    assert_eq!(hs.ut_pex_id(), Some(2));
    assert_eq!(hs.v(), Some(DEFAULT_CLIENT_VERSION));
}

#[test]
fn test_handshake_serialization_roundtrip() {
    let hs = ExtensionHandshake::new();
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.ut_metadata_id(), Some(1));
    assert_eq!(parsed.ut_pex_id(), Some(2));
    assert_eq!(parsed.v(), Some(DEFAULT_CLIENT_VERSION));
    assert_eq!(parsed.metadata_size(), None);
    assert_eq!(parsed.port(), None);
}

#[test]
fn test_handshake_with_custom_ext_ids() {
    let mut hs = ExtensionHandshake::new();
    hs.with_ut_metadata(5).with_ut_pex(7);
    assert_eq!(hs.ut_metadata_id(), Some(5));
    assert_eq!(hs.ut_pex_id(), Some(7));

    // Roundtrip
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.ut_metadata_id(), Some(5));
    assert_eq!(parsed.ut_pex_id(), Some(7));
}

#[test]
fn test_handshake_without_m_key_has_no_extension_ids() {
    let mut dict = BTreeMap::new();
    dict.insert(b"reqq".to_vec(), BencodeValue::Int(500));
    let bytes = BencodeValue::Dict(dict).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.ut_metadata_id(), None);
    assert_eq!(parsed.ut_pex_id(), None);
    assert_eq!(parsed.reqq(), 500);
}

#[test]
fn test_handshake_m_not_dict_keeps_other_fields() {
    let mut dict = BTreeMap::new();
    dict.insert(b"m".to_vec(), BencodeValue::Int(42));
    dict.insert(
        b"v".to_vec(),
        BencodeValue::Bytes(b"remote-agent/2.3".to_vec()),
    );
    dict.insert(b"reqq".to_vec(), BencodeValue::Int(17));
    let bytes = BencodeValue::Dict(dict).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.ut_metadata_id(), None);
    assert_eq!(parsed.ut_pex_id(), None);
    assert_eq!(parsed.v(), Some("remote-agent/2.3"));
    assert_eq!(parsed.reqq(), 17);
}

#[test]
fn test_handshake_missing_ut_metadata() {
    // m dict without ut_metadata — ut_metadata_id returns None
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(3));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert!(parsed.ut_metadata_id().is_none());
    assert_eq!(parsed.ut_pex_id(), Some(3));
}

#[test]
fn test_handshake_missing_ut_pex() {
    // m dict without ut_pex — ut_pex_id returns None
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.ut_metadata_id(), Some(1));
    assert!(parsed.ut_pex_id().is_none());
}

#[test]
fn test_handshake_invalid_bencode() {
    let result = ExtensionHandshake::from_bytes(b"not bencoded");
    assert!(result.is_err());
}

#[test]
fn test_handshake_empty_input() {
    let result = ExtensionHandshake::from_bytes(b"");
    assert!(result.is_err());
}

#[test]
fn test_handshake_bencode_wire_format() {
    // Verify the actual bytes produced match expected bencode structure
    let hs = ExtensionHandshake::new();
    let bytes = hs.to_bytes();

    // Must start with 'd' and end with 'e'
    assert_eq!(bytes[0], b'd');
    assert_eq!(bytes[bytes.len() - 1], b'e');

    // Must contain "ut_metadata" and "ut_pex" keys
    let s = String::from_utf8_lossy(&bytes);
    assert!(s.contains("ut_metadata"));
    assert!(s.contains("ut_pex"));
    assert!(s.contains("reqq"));
    // Default handshake includes client version string
    assert!(s.contains(DEFAULT_CLIENT_VERSION));
    // No metadata_size or port in default handshake
    assert!(!s.contains("metadata_size"));
}

#[test]
fn test_handshake_reqq_parsed_from_peer() {
    // A peer sends reqq=1000
    let mut hs = ExtensionHandshake::new();
    hs.with_reqq(1000);
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.reqq(), 1000);
}

#[test]
fn test_handshake_reqq_default_when_missing() {
    // A peer sends handshake without reqq — we should default to 500
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(2));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    // No reqq key
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.reqq(), 500);
}

// ======================== New field tests (v, metadata_size, port) ========================

#[test]
fn test_handshake_version_roundtrip() {
    let mut hs = ExtensionHandshake::new();
    hs.with_version("test-peer/1.0");
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.v(), Some("test-peer/1.0"));
}

#[test]
fn test_handshake_version_default() {
    let hs = ExtensionHandshake::new();
    assert_eq!(hs.v(), Some(DEFAULT_CLIENT_VERSION));
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.v(), Some(DEFAULT_CLIENT_VERSION));
}

#[test]
fn test_handshake_without_version() {
    let mut hs = ExtensionHandshake::new();
    hs.without_version();
    assert_eq!(hs.v(), None);
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.v(), None);
}

#[test]
fn test_handshake_metadata_size_roundtrip() {
    let mut hs = ExtensionHandshake::new();
    hs.with_metadata_size(12345);
    assert_eq!(hs.metadata_size(), Some(12345));
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.metadata_size(), Some(12345));
}

#[test]
fn test_handshake_metadata_size_not_present() {
    let hs = ExtensionHandshake::new();
    assert_eq!(hs.metadata_size(), None);
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.metadata_size(), None);
}

#[test]
fn test_handshake_metadata_size_zero_rejected() {
    // metadata_size=0 should be treated as absent (C++ checks size > 0)
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"metadata_size".to_vec(), BencodeValue::Int(0));
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.metadata_size(), None);
}

#[test]
fn test_handshake_metadata_size_negative_rejected() {
    // Negative metadata_size should be rejected (C++ throws on negative)
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"metadata_size".to_vec(), BencodeValue::Int(-1));
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.metadata_size(), None);
}

#[test]
fn test_handshake_metadata_size_max_8mib() {
    // 8 MiB is the maximum accepted metadata_size (C++ aria2 limit)
    let max_size = MAX_UT_METADATA_SIZE;
    let mut hs = ExtensionHandshake::new();
    hs.with_metadata_size(max_size);
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.metadata_size(), Some(max_size));
}

#[test]
fn test_handshake_metadata_size_over_8mib_rejected() {
    // metadata_size > 8 MiB should be rejected
    let oversized = MAX_UT_METADATA_SIZE + 1;
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(
        b"metadata_size".to_vec(),
        BencodeValue::Int(oversized as i64),
    );
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.metadata_size(), None);
}

#[test]
fn test_handshake_port_roundtrip() {
    let mut hs = ExtensionHandshake::new();
    hs.with_port(6881);
    assert_eq!(hs.port(), Some(6881));
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.port(), Some(6881));
}

#[test]
fn test_handshake_port_not_present() {
    let hs = ExtensionHandshake::new();
    assert_eq!(hs.port(), None);
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.port(), None);
}

#[test]
fn test_handshake_port_zero_rejected() {
    // port=0 should be treated as absent (C++ checks port > 0)
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"p".to_vec(), BencodeValue::Int(0));
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.port(), None);
}

#[test]
fn test_handshake_port_negative_rejected() {
    // Negative port should be rejected
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"p".to_vec(), BencodeValue::Int(-1));
    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.port(), None);
}

#[test]
fn test_handshake_port_max_valid() {
    // port=65535 should be accepted
    let mut hs = ExtensionHandshake::new();
    hs.with_port(65535);
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.port(), Some(65535));
}

#[test]
fn test_handshake_all_fields_roundtrip() {
    // Full handshake with all fields set — mirrors real C++ aria2 output
    let mut hs = ExtensionHandshake::new();
    hs.with_version("test-peer/1.0")
        .with_port(6881)
        .with_metadata_size(45678)
        .with_reqq(1000)
        .with_ut_metadata(3)
        .with_ut_pex(4);

    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();

    assert_eq!(parsed.v(), Some("test-peer/1.0"));
    assert_eq!(parsed.port(), Some(6881));
    assert_eq!(parsed.metadata_size(), Some(45678));
    assert_eq!(parsed.reqq(), 1000);
    assert_eq!(parsed.ut_metadata_id(), Some(3));
    assert_eq!(parsed.ut_pex_id(), Some(4));
}

#[test]
fn test_handshake_peer_message_with_all_fields() {
    // Simulate a peer message with all BEP 10 fields, matching C++ wire format
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(2));
    m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(3));

    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(
        b"v".to_vec(),
        BencodeValue::Bytes(b"qBittorrent/4.5.2".to_vec()),
    );
    root.insert(b"p".to_vec(), BencodeValue::Int(6969));
    root.insert(b"metadata_size".to_vec(), BencodeValue::Int(98765));
    root.insert(b"reqq".to_vec(), BencodeValue::Int(250));

    let bytes = BencodeValue::Dict(root).encode();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();

    assert_eq!(parsed.v(), Some("qBittorrent/4.5.2"));
    assert_eq!(parsed.port(), Some(6969));
    assert_eq!(parsed.metadata_size(), Some(98765));
    assert_eq!(parsed.reqq(), 250);
    assert_eq!(parsed.ut_metadata_id(), Some(2));
    assert_eq!(parsed.ut_pex_id(), Some(3));
}
