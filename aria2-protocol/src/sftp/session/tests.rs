use super::*;
use crate::sftp::packet::status_code_description;
use crate::sftp::packet::{SSH_FX_OK, SftpFileAttrs};

#[test]
fn test_version_constants() {
    const _: () = assert!(SFTP_VERSION_MIN <= SFTP_VERSION_MAX);
    assert_eq!(SFTP_VERSION_MIN, 3);
    assert_eq!(SFTP_VERSION_MAX, 6);
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
