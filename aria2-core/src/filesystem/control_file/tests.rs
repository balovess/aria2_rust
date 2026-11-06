use super::native::PROJECT_EXTENSION_MAGIC;
use super::persistence::{CONTROL_HEADER_LEN, CONTROL_MAGIC, CONTROL_VERSION, FLAG_HAS_CHECKSUM};
use super::{ControlFile, ControlFileInFlightPiece};
use std::path::{Path, PathBuf};

#[tokio::test]
async fn original_aria2_bt_control_file_fixtures_restore_verified_progress() {
    const INFO_HASH: [u8; 20] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x00, 0xff, 0xff, 0xff, 0xff,
    ];
    // Keep these upstream aria2 test fixtures in-tree so tests do not
    // depend on the locally ignored aria2_original checkout.
    const V0000_FIXTURE: [u8; 94] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x14, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
        0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0xff, 0xff, 0xff, 0xff,
        0x00, 0x04, 0x00, 0x00, 0x00, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xfe, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00,
    ];
    const V0001_FIXTURE: [u8; 94] = [
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x14, 0x11, 0x22, 0x33, 0x44, 0x55,
        0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0xff, 0xff, 0xff, 0xff,
        0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x40, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xfe, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x04,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x02, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00,
    ];
    let fixtures: [(&str, &[u8]); 2] = [("v0000", &V0000_FIXTURE), ("v0001", &V0001_FIXTURE)];
    let dir = tempfile::tempdir().unwrap();

    for (version, bytes) in fixtures {
        let path = dir.path().join(format!("original-{version}.aria2"));
        tokio::fs::write(&path, bytes).await.unwrap();
        let loaded = ControlFile::load(&path).await.unwrap().unwrap();

        assert!(loaded.uses_native_layout(), "version {version}");
        assert!(loaded.is_torrent_checkpoint(), "version {version}");
        assert_eq!(
            loaded.torrent_info_hash(),
            Some(INFO_HASH),
            "version {version}"
        );
        assert_eq!(loaded.piece_length(), Some(1024), "version {version}");
        assert_eq!(loaded.total_length(), 80 * 1024, "version {version}");
        assert_eq!(loaded.upload_length(), 1024, "version {version}");
        assert_eq!(loaded.completed_length(), 79 * 1024, "version {version}");
        assert_eq!(
            loaded.bitfield(),
            &[0xff; 9].into_iter().chain([0xfe]).collect::<Vec<_>>()
        );
        assert_eq!(
            loaded.in_flight_pieces(),
            &[
                ControlFileInFlightPiece {
                    index: 1,
                    length: 1024,
                    bitfield: vec![0],
                },
                ControlFileInFlightPiece {
                    index: 2,
                    length: 512,
                    bitfield: vec![0],
                },
            ],
            "version {version}"
        );
    }
}

#[tokio::test]
async fn native_control_file_roundtrips_in_flight_block_bitfields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial-piece.bin.aria2");
    let mut control = ControlFile::open_or_create_with_piece_length(&path, 64 * 1024, 64 * 1024)
        .await
        .unwrap();
    control.mark_torrent_checkpoint();
    control.set_torrent_info_hash([0x31; 20]);
    control.set_in_flight_pieces(vec![ControlFileInFlightPiece {
        index: 0,
        length: 64 * 1024,
        bitfield: vec![0b1010_0000],
    }]);
    control.save().await.unwrap();

    let restored = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(restored.in_flight_pieces(), control.in_flight_pieces());
    assert_eq!(restored.bitfield(), &[0]);
}

#[test]
fn test_control_path_uses_aria2_suffix() {
    assert_eq!(
        ControlFile::control_path_for(Path::new("payload.bin")),
        PathBuf::from("payload.bin.aria2")
    );
}

#[test]
fn test_control_path_for_current_directory_is_a_sidecar_file() {
    let path = ControlFile::control_path_for(Path::new("."));
    assert_ne!(path, Path::new("."));
    assert_eq!(path.file_name(), Some(std::ffi::OsStr::new(".aria2")));
}

#[test]
fn test_control_path_appends_after_multiple_extensions() {
    assert_eq!(
        ControlFile::control_path_for(Path::new("archive.tar.gz")),
        PathBuf::from("archive.tar.gz.aria2")
    );
}

#[test]
fn test_control_path_for_multi_file_top_directory_is_a_sibling() {
    let path = ControlFile::control_path_for(Path::new("downloads/torrent"));
    assert_eq!(path, PathBuf::from("downloads/torrent.aria2"));
}

#[tokio::test]
async fn test_control_file_new_and_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.aria2");

    let cf = ControlFile::open_or_create(&path, 10000, 10).await.unwrap();
    assert_eq!(cf.total_length(), 10000);
    assert_eq!(cf.completed_length(), 0);
    assert!(!cf.is_piece_done(0));

    cf.save().await.unwrap();

    assert!(path.exists());
    let data = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&data[0..2], &1u16.to_be_bytes());
    assert_ne!(&data[0..4], CONTROL_MAGIC);
    assert!(
        data.windows(PROJECT_EXTENSION_MAGIC.len())
            .any(|window| { window == PROJECT_EXTENSION_MAGIC })
    );
}

#[tokio::test]
async fn fixed_piece_checkpoint_persists_the_exact_piece_layout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixed-piece.bin.aria2");
    let total_length: u64 = 101 * 1024 * 1024;
    let piece_length = 2 * 1024 * 1024;
    let pieces = total_length.div_ceil(piece_length as u64) as usize;

    let checkpoint =
        ControlFile::open_or_create_with_piece_length(&path, total_length, piece_length)
            .await
            .unwrap();
    assert_eq!(checkpoint.piece_length(), Some(piece_length));
    assert_eq!(checkpoint.bitfield().len(), pieces.div_ceil(8));
    checkpoint.save().await.unwrap();

    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.piece_length(), Some(piece_length));
    assert_eq!(loaded.bitfield().len(), pieces.div_ceil(8));
    assert!(
        ControlFile::open_or_create_with_piece_length(&path, total_length, piece_length / 2,)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_loads_native_aria2_v1_control_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.bin.aria2");
    let mut data = Vec::new();
    data.extend_from_slice(&1u16.to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(&2u32.to_be_bytes());
    data.extend_from_slice(&3u64.to_be_bytes());
    data.extend_from_slice(&7u64.to_be_bytes());
    data.extend_from_slice(&1u32.to_be_bytes());
    data.push(0b1100_0000);
    data.extend_from_slice(&0u32.to_be_bytes());
    tokio::fs::write(&path, data).await.unwrap();

    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.total_length(), 3);
    assert_eq!(loaded.upload_length(), 7);
    assert_eq!(loaded.completed_length(), 3);
    assert_eq!(loaded.bitfield(), &[0b1100_0000]);
    assert!(!loaded.is_torrent_checkpoint());
}

#[tokio::test]
async fn test_loads_native_aria2_v0_control_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native-v0.bin.aria2");
    let mut data = Vec::new();
    data.extend_from_slice(&0u16.to_ne_bytes());
    data.extend_from_slice(&0u32.to_ne_bytes());
    data.extend_from_slice(&0u32.to_ne_bytes());
    data.extend_from_slice(&4u32.to_ne_bytes());
    data.extend_from_slice(&4u64.to_ne_bytes());
    data.extend_from_slice(&0u64.to_ne_bytes());
    data.extend_from_slice(&1u32.to_ne_bytes());
    data.push(0b1000_0000);
    data.extend_from_slice(&0u32.to_ne_bytes());
    tokio::fs::write(&path, data).await.unwrap();

    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.total_length(), 4);
    assert_eq!(loaded.completed_length(), 4);
}

#[tokio::test]
async fn test_control_file_mark_and_check_pieces() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.aria2");

    let mut cf = ControlFile::open_or_create(&path, 1000, 8).await.unwrap();

    cf.mark_piece_done(0);
    cf.mark_piece_done(3);
    cf.mark_piece_done(7);

    assert!(cf.is_piece_done(0));
    assert!(!cf.is_piece_done(1));
    assert!(cf.is_piece_done(3));
    assert!(!cf.is_piece_done(5));
    assert!(cf.is_piece_done(7));
    assert_eq!(cf.completed_pieces(), 3);

    cf.save().await.unwrap();

    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.completed_pieces(), 3);
    assert!(loaded.is_piece_done(0));
    assert!(loaded.is_piece_done(7));
}

#[tokio::test]
async fn test_control_file_piece_completion_handles_short_final_piece() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short_final.aria2");

    let mut cf = ControlFile::open_or_create(&path, 10, 3).await.unwrap();
    cf.mark_piece_done(0);
    cf.mark_piece_done(2);

    assert_eq!(cf.completed_length(), 6);
    assert!(!cf.is_piece_done(3));
}

#[tokio::test]
async fn test_control_file_reload_restores_logical_piece_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reload_piece_count.aria2");

    let mut cf = ControlFile::open_or_create(&path, 10, 3).await.unwrap();
    cf.mark_piece_done(0);
    cf.save().await.unwrap();

    let mut loaded = ControlFile::open_or_create(&path, 10, 3).await.unwrap();
    loaded.mark_piece_done(2);

    assert_eq!(loaded.completed_length(), 6);
    assert!(!loaded.is_piece_done(3));
}

#[tokio::test]
async fn test_control_file_reload_normalizes_legacy_piece_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("normalize_piece_count.aria2");

    // This is the retired A2CF layout. It intentionally exercises the
    // compatibility reader instead of the current official writer.
    let mut legacy = Vec::new();
    legacy.extend_from_slice(CONTROL_MAGIC);
    legacy.extend_from_slice(&CONTROL_VERSION.to_le_bytes());
    legacy.push(0);
    legacy.extend_from_slice(&10u64.to_le_bytes());
    legacy.extend_from_slice(&6u64.to_le_bytes());
    legacy.extend_from_slice(&0u64.to_le_bytes());
    legacy.extend_from_slice(&1u64.to_le_bytes());
    legacy.push(0b1100_1000);
    tokio::fs::write(&path, legacy).await.unwrap();

    let loaded = ControlFile::open_or_create(&path, 10, 3).await.unwrap();

    assert_eq!(loaded.bitfield(), &[0b1100_0000]);
    assert_eq!(loaded.completed_pieces(), 2);
    assert!(loaded.is_piece_done(0));
    assert!(loaded.is_piece_done(1));
    assert!(!loaded.is_piece_done(2));
    assert!(!loaded.is_piece_done(3));
    assert_eq!(loaded.completed_length(), 8);
}

#[tokio::test]
async fn test_control_file_roundtrip_with_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_hash.aria2");

    let mut cf = ControlFile::open_or_create(&path, 5000, 5).await.unwrap();
    cf.checksum_algo = 2;
    cf.checksum_value = vec![0xAB; 20];
    cf.mark_piece_done(0);
    cf.mark_piece_done(2);
    cf.save().await.unwrap();

    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.total_length(), 5000);
    assert_eq!(loaded.checksum_algo, 2);
    assert_eq!(loaded.completed_pieces(), 2);
}

#[tokio::test]
async fn test_control_file_rejects_corrupt_project_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt_extension.aria2");
    let control = ControlFile::open_or_create(&path, 100, 4).await.unwrap();
    control.save().await.unwrap();

    let mut data = tokio::fs::read(&path).await.unwrap();
    *data.last_mut().unwrap() ^= 0x01;
    tokio::fs::write(&path, data).await.unwrap();

    assert!(ControlFile::load(&path).await.is_err());
}

#[tokio::test]
async fn test_control_file_atomic_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_atomic.aria2");

    let mut cf = ControlFile::open_or_create(&path, 999, 4).await.unwrap();
    cf.mark_piece_done(1);
    cf.save().await.unwrap();

    let tmp_path = path.with_extension("aria2.tmp");
    assert!(!tmp_path.exists());
    assert!(path.exists());
}

#[tokio::test]
async fn test_control_file_load_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent.aria2");
    let result = ControlFile::load(&path).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_control_file_load_invalid_magic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.aria2");
    tokio::fs::write(&path, b"NOT_A2CF_DATA").await.unwrap();

    let result = ControlFile::load(&path).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_control_file_load_truncated_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("truncated.aria2");
    for length in [0usize, 7, 8, 38] {
        let mut data = vec![0u8; length];
        if length >= 4 {
            data[..4].copy_from_slice(CONTROL_MAGIC);
        }
        tokio::fs::write(&path, &data).await.unwrap();
        assert!(ControlFile::load(&path).await.is_err(), "length={length}");
    }
}

#[tokio::test]
async fn test_control_file_rejects_invalid_checksum_and_bitfield_lengths() {
    let dir = tempfile::tempdir().expect("failed to create temporary directory");
    let path = dir.path().join("malformed.aria2");
    let mut data = vec![0u8; CONTROL_HEADER_LEN];
    data[..4].copy_from_slice(CONTROL_MAGIC);
    data[6] = FLAG_HAS_CHECKSUM;
    data[31..39].copy_from_slice(&0u64.to_le_bytes());
    data.push(2);
    tokio::fs::write(&path, &data).await.unwrap();
    assert!(ControlFile::load(&path).await.is_err());

    let mut data = vec![0u8; CONTROL_HEADER_LEN];
    data[..4].copy_from_slice(CONTROL_MAGIC);
    data[31..39].copy_from_slice(&1u64.to_le_bytes());
    tokio::fs::write(&path, &data).await.unwrap();
    assert!(ControlFile::load(&path).await.is_err());
}

#[tokio::test]
async fn test_control_path_for_output() {
    let out = Path::new("/downloads/file.iso");
    let ctrl = ControlFile::control_path_for(out);
    assert_eq!(ctrl.extension().unwrap().to_str().unwrap(), "aria2");
    assert_eq!(ctrl, PathBuf::from("/downloads/file.iso.aria2"));
}

#[tokio::test]
async fn test_control_file_update_completed_length() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_len.aria2");

    let mut cf = ControlFile::open_or_create(&path, 8000, 8).await.unwrap();
    cf.update_completed_length(3500);
    assert_eq!(cf.completed_length(), 3500);

    cf.update_completed_length(9000);
    assert_eq!(cf.completed_length(), 8000);

    cf.update_completed_length(3500);
    cf.save().await.unwrap();
    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.completed_length(), 3500);
}
