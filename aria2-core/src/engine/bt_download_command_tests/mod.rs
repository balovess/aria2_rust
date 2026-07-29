use crate::engine::bt_download_command::BtDownloadCommand;
use crate::request::request_group::{DownloadOptions, GroupId};

// Sub-modules for logically grouped test suites
mod choke_tracking;
mod multi_file_layout;
mod multi_file_write;
mod piece_provider;
mod private_torrent;

/// Build a minimal single-file public torrent (no `private` flag).
/// Shared across BT/magnet test modules to avoid duplicating bencode
/// fixtures.
pub(crate) fn build_test_torrent() -> Vec<u8> {
    let mut v = Vec::new();

    v.push(b'd');

    let url = b"http://tracker.example.com/announce";
    v.extend_from_slice(format!("8:announce{}:", url.len()).as_bytes());
    v.extend_from_slice(url);

    v.extend_from_slice(b"4:info");
    v.push(b'd');

    v.extend_from_slice(b"6:lengthi0e");

    v.extend_from_slice(b"4:name4:test");

    v.extend_from_slice(b"12:piece lengthi16384e");

    v.extend_from_slice(b"6:pieces20:");
    v.extend_from_slice(&[
        0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
        0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
    ]);

    v.push(b'e');

    v.push(b'e');

    v
}

pub(crate) fn create_test_command() -> BtDownloadCommand {
    let torrent_bytes = build_test_torrent();
    let options = DownloadOptions::default();
    let gid = GroupId::new(1);
    BtDownloadCommand::new(gid, &torrent_bytes, &options, None)
        .expect("Failed to create test command")
}

/// Build a minimal single-file torrent with the `private` flag set to 1
/// (BEP 0027). The info dict keys are emitted in sorted bencode order:
/// length, name, piece length, pieces, private.
/// Shared across BT/magnet test modules.
pub(crate) fn build_private_test_torrent() -> Vec<u8> {
    let mut v = Vec::new();

    v.push(b'd');

    let url = b"http://tracker.example.com/announce";
    v.extend_from_slice(format!("8:announce{}:", url.len()).as_bytes());
    v.extend_from_slice(url);

    v.extend_from_slice(b"4:info");
    v.push(b'd');

    v.extend_from_slice(b"6:lengthi0e");

    v.extend_from_slice(b"4:name4:test");

    v.extend_from_slice(b"12:piece lengthi16384e");

    v.extend_from_slice(b"6:pieces20:");
    v.extend_from_slice(&[
        0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
        0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
    ]);

    // private flag (BEP 0027): placed last in sorted key order
    v.extend_from_slice(b"7:privatei1e");

    v.push(b'e'); // close info dict
    v.push(b'e'); // close root dict

    v
}

pub(crate) fn create_private_test_command() -> BtDownloadCommand {
    let torrent_bytes = build_private_test_torrent();
    let options = DownloadOptions::default();
    let gid = GroupId::new(1);
    BtDownloadCommand::new(gid, &torrent_bytes, &options, None)
        .expect("Failed to create private test command")
}
