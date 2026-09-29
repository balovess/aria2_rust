use std::io;

use super::wire::{
    read_raw, read_string, read_u32, read_u64, write_string, write_string_raw, write_u32, write_u64,
};
use super::*;

#[test]
fn test_init_roundtrip() {
    let pkt = SftpPacket::Init { version: 3 };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_version_with_extensions_roundtrip() {
    let pkt = SftpPacket::Version {
        version: 3,
        extensions: vec![
            ("hardlink@openssh.com".to_string(), "1".to_string()),
            ("fsync@openssh.com".to_string(), "1".to_string()),
        ],
    };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_open_roundtrip() {
    let pkt = SftpPacket::Open {
        request_id: 42,
        filename: "/tmp/test.dat".to_string(),
        flags: SSH_FXF_READ | SSH_FXF_WRITE,
        attrs: SftpFileAttrs::default(),
    };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_status_roundtrip() {
    let pkt = SftpPacket::Status {
        request_id: 1,
        code: SSH_FX_OK,
        message: "OK".to_string(),
        language: "en".to_string(),
    };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_handle_roundtrip() {
    let pkt = SftpPacket::Handle {
        request_id: 5,
        handle: vec![0x01, 0x02, 0x03],
    };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_data_roundtrip() {
    let pkt = SftpPacket::Data {
        request_id: 10,
        data: b"hello world".to_vec(),
    };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_name_roundtrip() {
    let pkt = SftpPacket::Name {
        request_id: 20,
        entries: vec![SftpNameEntry {
            filename: "file1.txt".to_string(),
            longname: "-rw-r--r-- 1 user group 1234 Jan 1 00:00 file1.txt".to_string(),
            attrs: SftpFileAttrs::full(1234, 1000, 1000, 0o100644, 1700000000, 1700000100),
        }],
    };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_attrs_roundtrip() {
    let pkt = SftpPacket::Attrs {
        request_id: 30,
        attrs: SftpFileAttrs::full(4096, 0, 0, 0o040755, 1700000000, 1700000100),
    };
    let encoded = pkt.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, pkt);
}

#[test]
fn test_decode_incomplete_returns_error() {
    let buf = [0u8; 3]; // Too short for length prefix
    let result = SftpPacket::decode(&buf);
    assert!(result.is_err());
}

#[test]
fn test_decode_partial_payload_returns_error() {
    let pkt = SftpPacket::Init { version: 3 };
    let encoded = pkt.encode().unwrap();
    let result = SftpPacket::decode(&encoded[..5]); // Only length + 1 byte
    assert!(result.is_err());
}

#[test]
fn test_packet_type_codes() {
    assert_eq!(SftpPacket::Init { version: 3 }.packet_type(), SSH_FXP_INIT);
    assert_eq!(
        SftpPacket::Version {
            version: 3,
            extensions: vec![]
        }
        .packet_type(),
        SSH_FXP_VERSION
    );
    assert_eq!(
        SftpPacket::Open {
            request_id: 0,
            filename: String::new(),
            flags: 0,
            attrs: SftpFileAttrs::default(),
        }
        .packet_type(),
        SSH_FXP_OPEN
    );
}

#[test]
fn test_request_id_extraction() {
    assert!(SftpPacket::Init { version: 3 }.request_id().is_none());
    assert!(
        SftpPacket::Version {
            version: 3,
            extensions: vec![]
        }
        .request_id()
        .is_none()
    );
    assert_eq!(
        SftpPacket::Open {
            request_id: 42,
            filename: String::new(),
            flags: 0,
            attrs: SftpFileAttrs::default(),
        }
        .request_id(),
        Some(42)
    );
    assert_eq!(
        SftpPacket::Status {
            request_id: 99,
            code: SSH_FX_OK,
            message: String::new(),
            language: String::new(),
        }
        .request_id(),
        Some(99)
    );
}

#[test]
fn test_file_attrs_directory_check() {
    let dir_attrs = SftpFileAttrs::full(4096, 0, 0, 0o040755, 0, 0);
    assert!(dir_attrs.is_directory());
    assert!(!dir_attrs.is_regular_file());

    let file_attrs = SftpFileAttrs::full(100, 0, 0, 0o100644, 0, 0);
    assert!(file_attrs.is_regular_file());
    assert!(!file_attrs.is_directory());

    let link_attrs = SftpFileAttrs::full(10, 0, 0, 0o120777, 0, 0);
    assert!(link_attrs.is_symlink());
}

#[test]
fn test_status_code_descriptions() {
    assert_eq!(status_code_description(SSH_FX_OK), "Operation succeeded");
    assert_eq!(status_code_description(SSH_FX_EOF), "End of file");
    assert_eq!(status_code_description(999), "Unknown status code");
}

#[test]
fn test_write_and_read_roundtrip() {
    let mut buf = Vec::new();
    write_u32(&mut buf, 0xDEADBEEF).unwrap();
    write_u64(&mut buf, 0xCAFEBABE_CAFEBABE).unwrap();
    write_string(&mut buf, "hello").unwrap();
    write_string_raw(&mut buf, &[0x01, 0x02]).unwrap();

    let mut cursor = io::Cursor::new(&buf);
    assert_eq!(read_u32(&mut cursor).unwrap(), 0xDEADBEEF);
    assert_eq!(read_u64(&mut cursor).unwrap(), 0xCAFEBABE_CAFEBABE);
    assert_eq!(read_string(&mut cursor).unwrap(), "hello");
    assert_eq!(read_raw(&mut cursor).unwrap(), vec![0x01, 0x02]);
}
