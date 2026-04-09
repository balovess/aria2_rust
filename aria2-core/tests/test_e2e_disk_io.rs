mod fixtures {
    pub mod test_server;
}
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use aria2_core::filesystem::file_allocation::{self, preallocate_file};
use aria2_core::filesystem::resume_helper::ResumeHelper;

#[tokio::test]
async fn test_seekable_writer_basic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_basic.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(1024), None);
    writer.open().await.unwrap();
    assert!(writer.is_opened());

    writer.write_at(0, b"hello").await.unwrap();
    writer.write_at(5, b" world").await.unwrap();
    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&content[..11], b"hello world");
}

#[tokio::test]
async fn test_seekable_writer_random_access() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_random.bin");

    let mut writer = CachedDiskWriter::new(&path, None, None);
    writer.open().await.unwrap();

    writer.write_at(200, b"SEG2").await.unwrap();
    writer.write_at(0, b"SEG0").await.unwrap();
    writer.write_at(100, b"SEG1").await.unwrap();
    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), 204);
    assert_eq!(&content[0..4], b"SEG0");
    assert_eq!(&content[100..104], b"SEG1");
    assert_eq!(&content[200..204], b"SEG2");
}

#[tokio::test]
async fn test_preallocation_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_none.bin");

    preallocate_file(&path, 1024 * 1024, "none").await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn test_preallocation_trunc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_trunc.bin");

    preallocate_file(&path, 4096, "trunc").await.unwrap();

    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 4096);
}

#[tokio::test]
async fn test_preallocation_prealloc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_prealloc.bin");

    preallocate_file(&path, 1024 * 1024, "prealloc")
        .await
        .unwrap();

    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 1024 * 1024);
}

#[tokio::test]
async fn test_resume_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial_download.bin");

    tokio::fs::write(&path, vec![0xAB; 500]).await.unwrap();

    let helper = ResumeHelper::new(&path, true);
    let state = helper.detect(2000).await.unwrap();

    assert!(state.should_resume);
    assert_eq!(state.start_offset, 500);
    assert_eq!(state.existing_length, 500);

    let header = ResumeHelper::build_range_header(&state);
    assert_eq!(header, Some("bytes=500-".to_string()));
}

#[tokio::test]
async fn test_control_file_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_ctrl.aria2");

    let mut cf = ControlFile::open_or_create(&path, 8000, 8).await.unwrap();
    cf.mark_piece_done(0);
    cf.mark_piece_done(3);
    cf.mark_piece_done(7);
    cf.save().await.unwrap();

    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.total_length(), 8000);
    assert_eq!(loaded.completed_pieces(), 3);
    assert!(loaded.is_piece_done(0));
    assert!(loaded.is_piece_done(7));
    assert!(!loaded.is_piece_done(1));

    let tmp_path = path.with_extension("aria2.tmp");
    assert!(!tmp_path.exists());
}

#[tokio::test]
async fn test_control_file_with_checksum_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_hash_ctrl.aria2");

    let mut cf = ControlFile::open_or_create(&path, 5000, 5).await.unwrap();
    cf.set_checksum(2, vec![0xAB; 20]);
    cf.mark_piece_done(0);
    cf.mark_piece_done(2);
    cf.save().await.unwrap();

    let loaded = ControlFile::load(&path).await.unwrap().unwrap();
    assert_eq!(loaded.total_length(), 5000);
    assert_eq!(loaded.checksum_algo(), 2);
    assert_eq!(loaded.completed_pieces(), 2);
}

#[tokio::test]
async fn test_resume_complete_skip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("complete_file.bin");

    let expected_data = vec![0x42; 1024];
    tokio::fs::write(&path, &expected_data).await.unwrap();

    let helper = ResumeHelper::new(&path, true);
    let state = helper.detect(1024).await.unwrap();

    assert!(state.is_complete);
    assert!(!state.should_resume);
    assert_eq!(state.existing_length, 1024);
}

#[tokio::test]
async fn test_cached_writer_small_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_cached.bin");

    let mut writer = CachedDiskWriter::new(&path, None, Some(1));
    writer.open().await.unwrap();

    for i in 0u8..100 {
        let data = vec![i; 64];
        writer.write_at((i as u64) * 64, &data).await.unwrap();
    }

    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), 6400);

    for i in 0u8..100 {
        let start = i as usize * 64;
        assert_eq!(content[start], i, "mismatch at byte {}", start);
    }
}

#[tokio::test]
async fn test_disk_space_check_returns_value() {
    let dir = tempfile::tempdir().unwrap();
    let space = file_allocation::get_available_space(dir.path()).await;
    assert!(space.is_ok());
    assert!(space.unwrap() > 0);
}

#[tokio::test]
async fn test_preallocate_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub1").join("sub2").join("test_nested.bin");

    preallocate_file(&path, 256, "trunc").await.unwrap();

    assert!(path.exists());
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 256);
}

#[tokio::test]
async fn test_control_file_invalid_magic_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad_magic.aria2");

    tokio::fs::write(&path, b"NOT_A2CF_DATA").await.unwrap();

    let result = ControlFile::load(&path).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_resume_no_continue_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no_continue.bin");

    tokio::fs::write(&path, vec![0xCD; 300]).await.unwrap();

    let helper = ResumeHelper::new(&path, false);
    let state = helper.detect(2000).await.unwrap();

    assert!(!state.should_resume);
    assert_eq!(state.start_offset, 0);
}

#[tokio::test]
async fn test_seekable_writer_read_after_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_rw.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(256), None);
    writer.open().await.unwrap();
    writer.write_at(50, b"read-test-data").await.unwrap();
    writer.flush().await.unwrap();

    let mut buf = [0u8; 14];
    let n = writer.read_at(50, &mut buf).await.unwrap();
    assert_eq!(n, 14);
    assert_eq!(&buf, b"read-test-data");
}
