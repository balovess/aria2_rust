use super::*;
use crate::sftp::packet::status_code_description;
use crate::sftp::packet::{SSH_FX_OK, SftpFileAttrs};

#[test]
fn test_version_constants() {
    const _: () = assert!(SFTP_VERSION_MIN <= SFTP_VERSION_MAX);
    assert_eq!(SFTP_VERSION_MIN, 3);
    assert_eq!(SFTP_VERSION_MAX, 3);
}

#[test]
fn server_version_must_match_the_v3_wire_codec() {
    assert!(SftpSession::validate_server_version(3).is_ok());
    assert!(matches!(
        SftpSession::validate_server_version(2),
        Err(SftpSessionError::UnsupportedVersion {
            server_version: 2,
            minimum: 3,
        })
    ));
    assert!(matches!(
        SftpSession::validate_server_version(4),
        Err(SftpSessionError::UnsupportedVersionAboveMaximum {
            server_version: 4,
            maximum: 3,
        })
    ));
}

#[tokio::test]
async fn packet_read_timeout_covers_all_fragments() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    let packet = SftpPacket::Version {
        version: 3,
        extensions: Vec::new(),
    }
    .encode()
    .unwrap();
    let writer_task = tokio::spawn(async move {
        for chunk in packet.chunks(2) {
            tokio::time::sleep(Duration::from_millis(60)).await;
            writer.write_all(chunk).await.unwrap();
        }
    });

    let started_at = std::time::Instant::now();
    let result = SftpSession::recv_packet_from_reader(
        &mut reader,
        &mut Vec::new(),
        Duration::from_millis(140),
    )
    .await;
    assert!(
        matches!(result, Err(SftpSessionError::Timeout(timeout)) if timeout == Duration::from_millis(140))
    );
    assert!(started_at.elapsed() < Duration::from_millis(240));
    writer_task.abort();
}

#[tokio::test]
async fn oversized_frame_prefix_is_rejected_before_receiving_its_body() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    let payload_length = u32::try_from(crate::sftp::packet::MAX_SFTP_FRAME_SIZE - 3).unwrap();
    writer
        .write_all(&payload_length.to_be_bytes())
        .await
        .unwrap();

    let result =
        SftpSession::recv_packet_from_reader(&mut reader, &mut Vec::new(), Duration::from_secs(1))
            .await;

    assert!(matches!(
        result,
        Err(SftpSessionError::PacketDecode(error))
            if error.kind() == std::io::ErrorKind::InvalidData
    ));
}

#[test]
fn test_sftp_extension_creation_and_display() {
    let ext = SftpExtension {
        name: "hardlink@openssh.com".to_string(),
        data: "1".to_string(),
    };
    assert_eq!(ext.name, "hardlink@openssh.com");
    assert_eq!(ext.data, "1");
    assert_eq!(format!("{}", ext), "hardlink@openssh.com=1");
}

#[test]
fn test_sftp_extension_equality() {
    let ext1 = SftpExtension {
        name: "fsync@openssh.com".to_string(),
        data: "2".to_string(),
    };
    let ext2 = SftpExtension {
        name: "fsync@openssh.com".to_string(),
        data: "2".to_string(),
    };
    let ext3 = SftpExtension {
        name: "fsync@openssh.com".to_string(),
        data: "3".to_string(),
    };

    assert_eq!(ext1, ext2);
    assert_ne!(ext1, ext3);
}

// -----------------------------------------------------------------
// Set Request ID In Packet Tests
// -----------------------------------------------------------------

#[test]
fn test_set_request_id_for_all_packet_types() {
    // Test each request packet type accepts a request_id
    let cases: Vec<SftpPacket> = vec![
        SftpPacket::Open {
            request_id: 0,
            filename: "/tmp/test".into(),
            flags: 0,
            attrs: SftpFileAttrs::default(),
        },
        SftpPacket::Close {
            request_id: 0,
            handle: vec![1, 2, 3],
        },
        SftpPacket::Read {
            request_id: 0,
            handle: vec![1],
            offset: 0,
            length: 1024,
        },
        SftpPacket::Write {
            request_id: 0,
            handle: vec![1],
            offset: 0,
            data: vec![0],
        },
        SftpPacket::Stat {
            request_id: 0,
            path: "/test".into(),
        },
        SftpPacket::Lstat {
            request_id: 0,
            path: "/test".into(),
        },
        SftpPacket::Fstat {
            request_id: 0,
            handle: vec![1],
        },
        SftpPacket::Opendir {
            request_id: 0,
            path: "/dir".into(),
        },
        SftpPacket::Readdir {
            request_id: 0,
            handle: vec![1],
        },
        SftpPacket::Realpath {
            request_id: 0,
            path: ".".into(),
        },
        SftpPacket::Remove {
            request_id: 0,
            filename: "/file".into(),
        },
        SftpPacket::Mkdir {
            request_id: 0,
            path: "/new".into(),
            attrs: SftpFileAttrs::default(),
        },
        SftpPacket::Rmdir {
            request_id: 0,
            path: "/old".into(),
        },
        SftpPacket::Rename {
            request_id: 0,
            old_path: "/a".into(),
            new_path: "/b".into(),
        },
        SftpPacket::Readlink {
            request_id: 0,
            path: "/link".into(),
        },
        SftpPacket::Symlink {
            request_id: 0,
            link_path: "/l".into(),
            target_path: "/t".into(),
        },
    ];

    for mut pkt in cases {
        SftpSession::set_request_id_in_packet(&mut pkt, 42);
        assert_eq!(
            pkt.request_id(),
            Some(42),
            "Failed for {:?}",
            pkt.packet_type()
        );
    }
}

// -----------------------------------------------------------------
// Status Code Utility Tests
// -----------------------------------------------------------------

#[test]
fn test_status_code_descriptions_accessible() {
    // Verify status code descriptions are accessible from session module
    assert_eq!(status_code_description(SSH_FX_OK), "Operation succeeded");
    assert_eq!(status_code_description(1), "End of file"); // SSH_FX_EOF
}

#[test]
fn test_file_attrs_in_session_context() {
    // Test SftpFileAttrs creation as used in session context
    let attrs = SftpFileAttrs::full(4096, 1000, 1000, 0o040755, 1700000000, 1700000100);
    assert!(attrs.is_directory());
    assert_eq!(attrs.size, Some(4096));

    let flags = attrs.flags();
    assert!(flags != 0);
}
