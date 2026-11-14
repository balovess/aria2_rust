use super::*;
use crate::sftp::packet::{
    SSH_FILEXFER_ATTR_ACMODTIME, SSH_FILEXFER_ATTR_PERMISSIONS, SSH_FILEXFER_ATTR_SIZE,
    SSH_FXF_CREAT, SSH_FXF_READ, SSH_FXF_TRUNC, SSH_FXF_WRITE, SftpPacket,
};

#[test]
fn test_open_flags_readonly() {
    let flags = OpenFlags::readonly();
    assert!(flags.is_read());
    assert!(!flags.is_write());
    assert_eq!(flags.bits(), SSH_FXF_READ);
}

#[test]
fn test_open_flags_write_create() {
    let flags = OpenFlags::write_create();
    assert!(!flags.is_read());
    assert!(flags.is_write());
    assert!(flags.is_create());
    assert!(flags.is_trunc());
    assert_eq!(flags.bits(), SSH_FXF_WRITE | SSH_FXF_CREAT | SSH_FXF_TRUNC);
}

#[test]
fn test_open_flags_write_create_without_truncate() {
    let flags = OpenFlags::write_create_no_truncate();
    assert!(flags.is_write());
    assert!(flags.is_create());
    assert!(!flags.is_trunc());
    assert_eq!(flags.bits(), SSH_FXF_WRITE | SSH_FXF_CREAT);
}

#[test]
fn test_open_flags_read_write() {
    let flags = OpenFlags::read_write();
    assert!(flags.is_read());
    assert!(flags.is_write());
    assert!(!flags.is_append());
    assert_eq!(flags.bits(), SSH_FXF_READ | SSH_FXF_WRITE);
}

#[test]
fn test_open_flags_append() {
    let flags = OpenFlags::append();
    assert!(flags.is_write());
    assert!(flags.is_append());
    assert!(flags.is_create());
}

#[test]
fn test_open_flags_create_new() {
    let flags = OpenFlags::create_new();
    assert!(flags.is_write());
    assert!(flags.is_create());
    assert!(flags.is_excl());
    assert!(!flags.is_trunc());
}

#[test]
fn test_open_flags_from_bits() {
    let flags = OpenFlags::from_bits(0xFF);
    assert_eq!(flags.bits(), 0xFF);
}

#[test]
fn test_open_flags_display() {
    assert_eq!(OpenFlags::readonly().to_string(), "OPEN(READ)");
    assert_eq!(
        OpenFlags::write_create().to_string(),
        "OPEN(WRITE|CREAT|TRUNC)"
    );
    assert_eq!(OpenFlags::from_bits(0).to_string(), "OPEN(0x00000000)");
}

#[test]
fn test_file_attributes_default() {
    let attrs = FileAttributes::default();
    assert_eq!(attrs.size, 0);
    assert_eq!(attrs.permissions, 0);
    assert!(!attrs.is_regular_file);
    assert!(!attrs.is_directory);
    assert!(!attrs.is_symlink);
}

#[test]
fn test_file_attributes_from_wire_regular_file() {
    let wire = SftpFileAttrs::full(12345, 1000, 1000, 0o100644, 1700000000, 1700000100);
    let attrs = FileAttributes::from_wire(&wire);
    assert_eq!(attrs.size, 12345);
    assert_eq!(attrs.uid, 1000);
    assert_eq!(attrs.gid, 1000);
    assert_eq!(attrs.permissions, 0o100644);
    assert!(attrs.is_regular_file);
    assert!(!attrs.is_directory);
    assert!(!attrs.is_symlink);
}

#[test]
fn test_file_attributes_from_wire_directory() {
    let wire = SftpFileAttrs::full(4096, 0, 0, 0o040755, 0, 0);
    let attrs = FileAttributes::from_wire(&wire);
    assert!(attrs.is_directory);
    assert!(!attrs.is_regular_file);
}

#[test]
fn test_file_attributes_from_wire_symlink() {
    let wire = SftpFileAttrs::full(10, 0, 0, 0o120777, 0, 0);
    let attrs = FileAttributes::from_wire(&wire);
    assert!(attrs.is_symlink);
    assert!(!attrs.is_regular_file);
    assert!(!attrs.is_directory);
}

#[test]
fn test_file_attributes_to_wire_roundtrip() {
    let original = FileAttributes {
        size: 999,
        uid: 500,
        gid: 600,
        permissions: 0o100755,
        atime: 1700000000,
        mtime: 1700000100,
        is_regular_file: true,
        is_directory: false,
        is_symlink: false,
    };
    let wire = original.to_wire();
    let roundtrip = FileAttributes::from_wire(&wire);
    assert_eq!(roundtrip.size, original.size);
    assert_eq!(roundtrip.uid, original.uid);
    assert_eq!(roundtrip.gid, original.gid);
    assert_eq!(roundtrip.permissions, original.permissions);
    assert!(roundtrip.is_regular_file);
}

#[test]
fn test_file_attributes_display() {
    let attrs = FileAttributes {
        size: 1024,
        permissions: 0o100644,
        is_regular_file: true,
        ..Default::default()
    };
    let display = attrs.to_string();
    assert!(display.contains("file"));
    assert!(display.contains("1024"));
}

#[test]
fn test_file_attributes_permissions_only_roundtrip() {
    // This is the pattern used by the standalone transfer implementation for set_stat
    let attrs = FileAttributes {
        permissions: 0o644,
        ..Default::default()
    };
    let wire = attrs.to_wire();
    assert_eq!(wire.permissions, Some(0o644));
    assert_eq!(wire.size, Some(0)); // Default 0 is always populated in to_wire
}

#[test]
fn legacy_file_attributes_omit_incomplete_zero_valued_pairs() {
    let wire = FileAttributes {
        uid: 1000,
        gid: 0,
        atime: 100,
        mtime: 0,
        ..FileAttributes::default()
    }
    .to_wire();

    assert_eq!(
        wire.flags & crate::sftp::packet::SSH_FILEXFER_ATTR_UIDGID,
        0
    );
    assert_eq!(wire.flags & SSH_FILEXFER_ATTR_ACMODTIME, 0);
}

#[test]
fn file_attribute_patch_preserves_explicit_zero_values() {
    let wire = FileAttributePatch {
        size: Some(0),
        permissions: Some(0),
        timestamps: Some(FileTimestamps { atime: 0, mtime: 0 }),
        ..FileAttributePatch::default()
    }
    .to_wire();

    assert_eq!(
        wire.flags,
        SSH_FILEXFER_ATTR_SIZE | SSH_FILEXFER_ATTR_PERMISSIONS | SSH_FILEXFER_ATTR_ACMODTIME
    );
    assert_eq!(wire.size, Some(0));
    assert_eq!(wire.permissions, Some(0));
    assert_eq!(wire.atime, Some(0));
    assert_eq!(wire.mtime, Some(0));
    assert_eq!(wire.uid, None);
    assert_eq!(wire.gid, None);
}

#[test]
fn file_attribute_patch_sets_owner_ids_as_a_pair() {
    let wire = FileAttributePatch {
        owner: Some(FileOwner { uid: 0, gid: 0 }),
        ..FileAttributePatch::default()
    }
    .to_wire();

    assert_eq!(wire.flags, crate::sftp::packet::SSH_FILEXFER_ATTR_UIDGID);
    assert_eq!(wire.uid, Some(0));
    assert_eq!(wire.gid, Some(0));
}

#[test]
fn file_attribute_patch_roundtrips_explicit_zero_values_on_setstat_wire() {
    let packet = SftpPacket::Setstat {
        request_id: 7,
        path: "/file".to_string(),
        attrs: FileAttributePatch {
            size: Some(0),
            permissions: Some(0),
            timestamps: Some(FileTimestamps { atime: 0, mtime: 0 }),
            ..FileAttributePatch::default()
        }
        .to_wire(),
    };

    let encoded = packet.encode().unwrap();
    let (decoded, consumed) = SftpPacket::decode(&encoded).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, packet);
}
