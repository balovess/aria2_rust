//! Unit tests for DownloadContext and related types.

use std::thread;
use std::time::Duration;

use crate::download::file_entry::FileEntry;
use super::context::DownloadContext;
use super::types::{ContextAttributeType, Signature};

// Helper: create a FileEntry with given path, length, offset
fn make_file_entry(path: &str, length: u64, offset: u64) -> FileEntry {
    FileEntry::new(path.to_string(), length, offset, Vec::new())
}

// -----------------------------------------------------------------------
// 1. Default constructor
// -----------------------------------------------------------------------
#[test]
fn test_default_constructor() {
    let ctx = DownloadContext::new_default();
    assert_eq!(ctx.get_piece_length(), 0);
    assert!(ctx.knows_total_length());
    assert!(!ctx.is_checksum_verification_needed());
    assert!(!ctx.is_checksum_verification_available());
    assert!(!ctx.is_piece_hash_verification_available());
    assert!(ctx.get_accept_metalink());
    assert!(ctx.get_file_entries().is_empty());
    assert_eq!(ctx.get_total_length(), 0);
    assert!(ctx.get_signature().is_none());
    assert!(ctx.get_owner_request_group_id().is_none());
    // get_base_path() panics on empty file entries, so we skip it here
    // and test it in the base_path tests below.
}

// -----------------------------------------------------------------------
// 2. Parameterized constructor (pieceLength, totalLength, path)
// -----------------------------------------------------------------------
#[test]
fn test_parameterized_constructor() {
    let ctx = DownloadContext::new(1048576, 104857600, "/tmp/file.bin".into());
    assert_eq!(ctx.get_piece_length(), 1048576);
    assert_eq!(ctx.get_total_length(), 104857600);
    assert_eq!(ctx.get_file_entries().len(), 1);
    assert_eq!(ctx.get_first_file_entry().path(), "/tmp/file.bin");
    assert!(ctx.knows_total_length());
    assert!(!ctx.is_checksum_verification_needed());
}

// -----------------------------------------------------------------------
// 3. File entry management
// -----------------------------------------------------------------------
#[test]
fn test_file_entry_management() {
    let mut ctx = DownloadContext::new_default();

    // Add file entries
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
        make_file_entry("file3.bin", 3000, 3000),
    ]);

    assert_eq!(ctx.get_file_entries().len(), 3);
    assert_eq!(ctx.get_first_file_entry().path(), "file1.bin");
}

#[test]
fn test_get_first_requested_file_entry() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
    ]);

    // By default all are requested
    let first_req = ctx.get_first_requested_file_entry();
    assert!(first_req.is_some());
    assert_eq!(first_req.unwrap().path(), "file1.bin");

    // Mark first as not requested
    ctx.get_file_entries_mut()[0].set_requested(false);
    let first_req = ctx.get_first_requested_file_entry();
    assert!(first_req.is_some());
    assert_eq!(first_req.unwrap().path(), "file2.bin");
}

#[test]
fn test_count_requested_file_entry() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
        make_file_entry("file3.bin", 3000, 3000),
    ]);

    assert_eq!(ctx.count_requested_file_entry(), 3);

    ctx.get_file_entries_mut()[1].set_requested(false);
    assert_eq!(ctx.count_requested_file_entry(), 2);
}

// -----------------------------------------------------------------------
// 4. findFileEntryByOffset (binary search)
// -----------------------------------------------------------------------
#[test]
fn test_find_file_entry_by_offset() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),    // [0, 1000)
        make_file_entry("file2.bin", 2000, 1000), // [1000, 3000)
        make_file_entry("file3.bin", 3000, 3000), // [3000, 6000)
    ]);

    // Offset 0 -> first file
    let fe = ctx.find_file_entry_by_offset(0).unwrap();
    assert_eq!(fe.path(), "file1.bin");

    // Offset 500 -> first file
    let fe = ctx.find_file_entry_by_offset(500).unwrap();
    assert_eq!(fe.path(), "file1.bin");

    // Offset 1000 -> second file (exact boundary)
    let fe = ctx.find_file_entry_by_offset(1000).unwrap();
    assert_eq!(fe.path(), "file2.bin");

    // Offset 2500 -> second file
    let fe = ctx.find_file_entry_by_offset(2500).unwrap();
    assert_eq!(fe.path(), "file2.bin");

    // Offset 3000 -> third file (exact boundary)
    let fe = ctx.find_file_entry_by_offset(3000).unwrap();
    assert_eq!(fe.path(), "file3.bin");

    // Offset beyond range -> None
    assert!(ctx.find_file_entry_by_offset(6000).is_none());

    // Offset way beyond -> None
    assert!(ctx.find_file_entry_by_offset(99999).is_none());
}

#[test]
fn test_find_file_entry_by_offset_empty() {
    let ctx = DownloadContext::new_default();
    assert!(ctx.find_file_entry_by_offset(0).is_none());
}

// -----------------------------------------------------------------------
// 5. Total length derivation from file entries
// -----------------------------------------------------------------------
#[test]
fn test_total_length_derivation() {
    let mut ctx = DownloadContext::new_default();

    // Empty -> 0
    assert_eq!(ctx.get_total_length(), 0);

    // Single file
    ctx.set_file_entries(vec![make_file_entry("file.bin", 5000, 0)]);
    assert_eq!(ctx.get_total_length(), 5000);

    // Multiple files
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
    ]);
    // last_offset of last entry = 1000 + 2000 = 3000
    assert_eq!(ctx.get_total_length(), 3000);
}

// -----------------------------------------------------------------------
// 6. Piece hash management
// -----------------------------------------------------------------------
#[test]
fn test_piece_hash_management() {
    let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());

    assert!(ctx.get_piece_hash_type().is_empty());
    assert!(ctx.get_piece_hashes().is_empty());

    ctx.set_piece_hashes(
        "sha-1".to_string(),
        vec![
            "abc123".to_string(),
            "def456".to_string(),
            "ghi789".to_string(),
            "jkl012".to_string(),
        ],
    );

    assert_eq!(ctx.get_piece_hash_type(), "sha-1");
    assert_eq!(ctx.get_piece_hashes().len(), 4);
    assert_eq!(ctx.get_piece_hash(0), "abc123");
    assert_eq!(ctx.get_piece_hash(3), "jkl012");
}

#[test]
fn test_get_piece_hash_out_of_bounds() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_piece_hashes("sha-1".to_string(), vec!["abc".to_string()]);
    assert_eq!(ctx.get_piece_hash(5), "");
}

// -----------------------------------------------------------------------
// 7. getNumPieces calculation
// -----------------------------------------------------------------------
#[test]
fn test_get_num_pieces() {
    // 4096 bytes, 1024 piece length -> 4 pieces
    let ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
    assert_eq!(ctx.get_num_pieces(), 4);

    // 4097 bytes, 1024 piece length -> 5 pieces
    let ctx2 = DownloadContext::new(1024, 4097, "/tmp/file.bin".into());
    assert_eq!(ctx2.get_num_pieces(), 5);
}

#[test]
fn test_get_num_pieces_zero_piece_length() {
    let ctx = DownloadContext::new(0, 4096, "/tmp/file.bin".into());
    assert_eq!(ctx.get_num_pieces(), 0);
}

// -----------------------------------------------------------------------
// 8. Whole-file checksum management
// -----------------------------------------------------------------------
#[test]
fn test_whole_file_checksum() {
    let mut ctx = DownloadContext::new_default();

    assert!(ctx.get_digest().is_empty());
    assert!(ctx.get_hash_type().is_empty());

    ctx.set_digest("sha-256".to_string(), "abcdef1234567890".to_string());

    assert_eq!(ctx.get_hash_type(), "sha-256");
    assert_eq!(ctx.get_digest(), "abcdef1234567890");
}

// -----------------------------------------------------------------------
// 9. Verification availability checks
// -----------------------------------------------------------------------
#[test]
fn test_is_checksum_verification_needed() {
    let mut ctx = DownloadContext::new_default();

    // No digest/hash -> not needed
    assert!(!ctx.is_checksum_verification_needed());

    // Set digest+hash but no piece hash type -> needed
    ctx.set_digest("sha-256".to_string(), "abc".to_string());
    assert!(ctx.is_checksum_verification_needed());

    // Set piece hash type -> not needed (piece verification will handle it)
    ctx.set_piece_hashes("sha-1".to_string(), vec!["h1".to_string()]);
    assert!(!ctx.is_checksum_verification_needed());

    // Remove piece hash type, mark verified -> not needed
    let mut ctx2 = DownloadContext::new_default();
    ctx2.set_digest("sha-256".to_string(), "abc".to_string());
    ctx2.set_checksum_verified(true);
    assert!(!ctx2.is_checksum_verification_needed());
}

#[test]
fn test_is_checksum_verification_available() {
    let mut ctx = DownloadContext::new_default();
    assert!(!ctx.is_checksum_verification_available());

    ctx.set_digest("sha-256".to_string(), "abc".to_string());
    assert!(ctx.is_checksum_verification_available());
}

#[test]
fn test_is_checksum_verification_pending() {
    let mut ctx = DownloadContext::new_default();

    // Not available -> not pending
    assert!(!ctx.is_checksum_verification_pending());

    // Available but not verified -> pending
    ctx.set_digest("sha-256".to_string(), "abc".to_string());
    assert!(ctx.is_checksum_verification_pending());

    // Available and verified -> not pending
    ctx.set_checksum_verified(true);
    assert!(!ctx.is_checksum_verification_pending());
}

#[test]
fn test_is_checksum_verification_pending_with_piece_hash() {
    let mut ctx = DownloadContext::new_default();
    // Even with piece hash set, pending still returns true if
    // whole-file hash is available and not verified
    ctx.set_piece_hashes("sha-1".to_string(), vec!["h1".to_string()]);
    ctx.set_digest("sha-256".to_string(), "abc".to_string());
    // is_checksum_verification_needed would be false (piece hash type set),
    // but is_checksum_verification_pending is true (whole hash available, not verified)
    assert!(!ctx.is_checksum_verification_needed());
    assert!(ctx.is_checksum_verification_pending());
}

#[test]
fn test_is_piece_hash_verification_available() {
    let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
    assert!(!ctx.is_piece_hash_verification_available());

    // Set 3 piece hashes but need 4 -> not available
    ctx.set_piece_hashes(
        "sha-1".to_string(),
        vec!["h1".to_string(), "h2".to_string(), "h3".to_string()],
    );
    assert!(!ctx.is_piece_hash_verification_available());

    // Set 4 piece hashes matching numPieces -> available
    ctx.set_piece_hashes(
        "sha-1".to_string(),
        vec![
            "h1".to_string(),
            "h2".to_string(),
            "h3".to_string(),
            "h4".to_string(),
        ],
    );
    assert!(ctx.is_piece_hash_verification_available());
}

// -----------------------------------------------------------------------
// 10. BasePath with fallback to first FileEntry
// -----------------------------------------------------------------------
#[test]
fn test_base_path_fallback() {
    let ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
    // No base_path set -> falls back to first file entry's path
    assert_eq!(ctx.get_base_path(), "/tmp/file.bin");
}

#[test]
fn test_base_path_override() {
    let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
    ctx.set_base_path("/opt/download/file.bin".to_string());
    assert_eq!(ctx.get_base_path(), "/opt/download/file.bin");
}

// -----------------------------------------------------------------------
// 11. Piece length get/set
// -----------------------------------------------------------------------
#[test]
fn test_piece_length_get_set() {
    let mut ctx = DownloadContext::new_default();
    assert_eq!(ctx.get_piece_length(), 0);

    ctx.set_piece_length(262144);
    assert_eq!(ctx.get_piece_length(), 262144);
}

// -----------------------------------------------------------------------
// 12. knowsTotalLength / markTotalLengthIsKnown/Unknown
// -----------------------------------------------------------------------
#[test]
fn test_knows_total_length() {
    let mut ctx = DownloadContext::new_default();
    assert!(ctx.knows_total_length());

    ctx.mark_total_length_is_unknown();
    assert!(!ctx.knows_total_length());

    ctx.mark_total_length_is_known();
    assert!(ctx.knows_total_length());
}

// -----------------------------------------------------------------------
// 13. Accept metalink flag
// -----------------------------------------------------------------------
#[test]
fn test_accept_metalink() {
    let mut ctx = DownloadContext::new_default();
    assert!(ctx.get_accept_metalink());

    ctx.set_accept_metalink(false);
    assert!(!ctx.get_accept_metalink());

    ctx.set_accept_metalink(true);
    assert!(ctx.get_accept_metalink());
}

// -----------------------------------------------------------------------
// 14. Network stats (basic update)
// -----------------------------------------------------------------------
#[test]
fn test_network_stats_update() {
    let mut ctx = DownloadContext::new_default();

    ctx.update_download(100);
    ctx.update_download(200);
    assert_eq!(ctx.get_net_stat().session_download_length(), 300);

    ctx.update_upload_length(50);
    ctx.update_upload_length(25);
    assert_eq!(ctx.get_net_stat().session_upload_length(), 75);

    ctx.update_upload_speed(1024);
    assert_eq!(ctx.get_net_stat().upload_speed(), 1024);
}

// -----------------------------------------------------------------------
// 15. Release runtime resources
// -----------------------------------------------------------------------
#[test]
fn test_release_runtime_resource() {
    let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
    // Should not panic
    ctx.release_runtime_resource();
}

// -----------------------------------------------------------------------
// 16. File filter (setFileFilter with index list)
// -----------------------------------------------------------------------
#[test]
fn test_file_filter_empty_indices() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
    ]);

    // Empty filter -> all requested
    ctx.set_file_filter(vec![]);
    assert_eq!(ctx.count_requested_file_entry(), 2);
}

#[test]
fn test_file_filter_single_file() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![make_file_entry("file1.bin", 1000, 0)]);

    // Single file -> all requested regardless of filter
    ctx.set_file_filter(vec![5, 10]);
    assert_eq!(ctx.count_requested_file_entry(), 1);
}

#[test]
fn test_file_filter_selective() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
        make_file_entry("file3.bin", 3000, 3000),
    ]);

    // Select only file 2 (1-based index)
    ctx.set_file_filter(vec![2]);
    assert!(!ctx.get_file_entries()[0].is_requested());
    assert!(ctx.get_file_entries()[1].is_requested());
    assert!(!ctx.get_file_entries()[2].is_requested());
}

#[test]
fn test_file_filter_multiple_indices() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
        make_file_entry("file3.bin", 3000, 3000),
    ]);

    // Select files 1 and 3
    ctx.set_file_filter(vec![1, 3]);
    assert!(ctx.get_file_entries()[0].is_requested());
    assert!(!ctx.get_file_entries()[1].is_requested());
    assert!(ctx.get_file_entries()[2].is_requested());
}

// -----------------------------------------------------------------------
// 17. setFilePathWithIndex
// -----------------------------------------------------------------------
#[test]
fn test_set_file_path_with_index() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
    ]);

    assert!(ctx.set_file_path_with_index(1, "/new/path1.bin".into()).is_ok());
    assert_eq!(ctx.get_file_entries()[0].path(), "/new/path1.bin");

    assert!(ctx.set_file_path_with_index(2, "/new/path2.bin".into()).is_ok());
    assert_eq!(ctx.get_file_entries()[1].path(), "/new/path2.bin");
}

#[test]
fn test_set_file_path_with_index_out_of_bounds() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_file_entries(vec![make_file_entry("file1.bin", 1000, 0)]);

    // Index 0 is invalid
    assert!(ctx.set_file_path_with_index(0, "path".into()).is_err());

    // Index beyond length
    assert!(ctx.set_file_path_with_index(5, "path".into()).is_err());
}

// -----------------------------------------------------------------------
// 18. Checksum verified flag
// -----------------------------------------------------------------------
#[test]
fn test_checksum_verified_flag() {
    let mut ctx = DownloadContext::new_default();
    assert!(!ctx.is_checksum_verification_available());
    // By default not verified (but also not available, so "needed" is false)
    assert!(!ctx.is_checksum_verification_needed());

    ctx.set_digest("sha-256".to_string(), "abc".to_string());
    // Available and not verified -> needed (no piece hash type)
    assert!(ctx.is_checksum_verification_needed());

    ctx.set_checksum_verified(true);
    assert!(!ctx.is_checksum_verification_needed());
}

// -----------------------------------------------------------------------
// 19. Signature get/set
// -----------------------------------------------------------------------
#[test]
fn test_signature_get_set() {
    let mut ctx = DownloadContext::new_default();
    assert!(ctx.get_signature().is_none());

    ctx.set_signature(Signature::new(
        "-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----".to_string(),
        "sha-256".to_string(),
    ));

    let sig = ctx.get_signature().unwrap();
    assert_eq!(sig.hash_type, "sha-256");
    assert!(sig.body.contains("BEGIN PGP"));
}

// -----------------------------------------------------------------------
// 20. Timing (resetDownloadStartTime, resetDownloadStopTime, calculateSessionTime)
// -----------------------------------------------------------------------
#[test]
fn test_timing_start_stop_session() {
    let mut ctx = DownloadContext::new_default();

    // Before any timing operations
    assert!(ctx.get_download_stop_time().is_none());
    assert_eq!(ctx.calculate_session_time(), Duration::ZERO);

    // Start
    ctx.reset_download_start_time();
    assert!(ctx.get_net_stat().download_start_time().is_some());

    // Simulate some passage of time
    thread::sleep(Duration::from_millis(50));

    // Stop
    ctx.reset_download_stop_time();
    assert!(ctx.get_download_stop_time().is_some());

    // Session time should be at least 50ms
    let session = ctx.calculate_session_time();
    assert!(session >= Duration::from_millis(50));
}

#[test]
fn test_timing_reset_clears_stop() {
    let mut ctx = DownloadContext::new_default();

    ctx.reset_download_start_time();
    thread::sleep(Duration::from_millis(10));
    ctx.reset_download_stop_time();
    assert!(ctx.get_download_stop_time().is_some());

    // Reset start should clear stop time
    ctx.reset_download_start_time();
    assert!(ctx.get_download_stop_time().is_none());
}

// -----------------------------------------------------------------------
// Attributes
// -----------------------------------------------------------------------
#[test]
fn test_attributes() {
    let mut ctx = DownloadContext::new_default();

    assert!(!ctx.has_attribute(ContextAttributeType::BitTorrent));

    ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(42u64));
    assert!(ctx.has_attribute(ContextAttributeType::BitTorrent));

    let attr = ctx.get_attribute(ContextAttributeType::BitTorrent);
    assert!(attr.is_some());
    let val = attr.unwrap().downcast_ref::<u64>();
    assert!(val.is_some());
    assert_eq!(*val.unwrap(), 42u64);

    assert!(!ctx.has_attribute(ContextAttributeType::Ed2k));
}

// -----------------------------------------------------------------------
// Owner request group ID
// -----------------------------------------------------------------------
#[test]
fn test_owner_request_group_id() {
    let mut ctx = DownloadContext::new_default();
    assert!(ctx.get_owner_request_group_id().is_none());

    ctx.set_owner_request_group_id(42);
    assert_eq!(ctx.get_owner_request_group_id(), Some(42));
}

// -----------------------------------------------------------------------
// Edge cases
// -----------------------------------------------------------------------
#[test]
#[should_panic(expected = "get_first_file_entry: no file entries")]
fn test_get_first_file_entry_panics_on_empty() {
    let ctx = DownloadContext::new_default();
    let _ = ctx.get_first_file_entry();
}

#[test]
fn test_num_pieces_with_multiple_files() {
    let mut ctx = DownloadContext::new_default();
    ctx.set_piece_length(1024);
    // Two files: [0, 1000) + [1000, 3000) -> last_offset = 3000
    ctx.set_file_entries(vec![
        make_file_entry("file1.bin", 1000, 0),
        make_file_entry("file2.bin", 2000, 1000),
    ]);
    // (3000 + 1024 - 1) / 1024 = 3
    assert_eq!(ctx.get_num_pieces(), 3);
}

#[test]
fn test_default_trait() {
    let ctx = DownloadContext::default();
    assert_eq!(ctx.get_piece_length(), 0);
    assert!(ctx.knows_total_length());
    assert!(ctx.get_file_entries().is_empty());
}

#[test]
fn test_set_file_entries_replaces() {
    let mut ctx = DownloadContext::new(1024, 4096, "/tmp/old.bin".into());
    assert_eq!(ctx.get_file_entries().len(), 1);

    ctx.set_file_entries(vec![
        make_file_entry("new1.bin", 500, 0),
        make_file_entry("new2.bin", 500, 500),
    ]);
    assert_eq!(ctx.get_file_entries().len(), 2);
    assert_eq!(ctx.get_first_file_entry().path(), "new1.bin");
}
