use std::sync::Arc;

use super::*;
use crate::checksum::message_digest::HashType;

// ── Test fixture helpers ─────────────────────────────────────────────────

/// Create a test `Arc<DownloadContext>` with the given piece length and total length.
fn make_dctx(piece_length: u32, total_length: u64) -> Arc<crate::download::DownloadContext> {
    Arc::new(crate::download::DownloadContext::new(
        piece_length,
        total_length,
        "/tmp/test_check_integrity.bin".to_string(),
    ))
}

/// Create a test `Arc<dyn PieceStorage>` with the given piece length and total length.
fn make_ps(
    piece_length: u64,
    total_length: u64,
) -> Arc<dyn crate::segment::piece_storage::PieceStorage> {
    Arc::new(crate::segment::piece_storage::DefaultPieceStorage::new(
        piece_length,
        total_length,
    ))
}

// ── ValidatorKind enum dispatch tests ─────────────────────────────────

#[test]
fn test_validator_kind_none_is_finished() {
    let v = ValidatorKind::None;
    assert!(
        v.is_finished(),
        "None variant should be finished (nothing to validate)"
    );
}

#[test]
fn test_validator_kind_none_zero_metrics() {
    let v = ValidatorKind::None;
    assert_eq!(v.current_offset(), 0);
    assert_eq!(v.total_length(), 0);
}

#[test]
fn test_validator_kind_none_init_and_validate_noop() {
    let mut v = ValidatorKind::None;
    // Should not panic or change state
    v.init();
    v.validate_chunk();
    assert!(v.is_finished());
}

#[test]
fn test_validator_kind_piece_hash_dispatch() {
    let ctx = make_dctx(1_048_576, 5_242_880);
    let ps = make_ps(1_048_576, 5_242_880);
    let v = ValidatorKind::PieceHash(PieceHashValidator::new(ctx, ps, 5, 5_242_880, 1_048_576));
    assert!(
        !v.is_finished(),
        "PieceHash with 5 pieces should not be finished initially"
    );
    assert_eq!(v.total_length(), 5_242_880);
    assert_eq!(v.current_offset(), 0);
}

// ── PieceHashValidator init and state tracking tests ──────────────────

#[test]
fn test_piece_hash_validator_new() {
    let ctx = make_dctx(1_048_576, 4_194_304);
    let ps = make_ps(1_048_576, 4_194_304);
    let v = PieceHashValidator::new(ctx, ps, 4, 4_194_304, 1_048_576);
    assert_eq!(v.current_piece_index(), 0);
    assert_eq!(v.total_pieces(), 4);
    assert!(!v.is_finished());
    assert_eq!(v.current_offset(), 0);
    assert_eq!(v.total_length(), 4_194_304);
}

#[test]
fn test_piece_hash_validator_zero_pieces_is_finished() {
    let ctx = make_dctx(1_048_576, 0);
    let ps = make_ps(1_048_576, 0);
    let v = PieceHashValidator::new(ctx, ps, 0, 0, 1_048_576);
    assert!(
        v.is_finished(),
        "Zero pieces should be immediately finished"
    );
}

#[test]
fn test_piece_hash_validator_init_resets_state() {
    let ctx = make_dctx(1_048_576, 3_145_728);
    let ps = make_ps(1_048_576, 3_145_728);
    let mut v = PieceHashValidator::new(ctx, ps, 3, 3_145_728, 1_048_576);
    // Simulate partial progress
    v.validate_chunk();
    v.validate_chunk();
    assert_eq!(v.current_piece_index(), 2);

    // Init should reset
    v.init();
    assert_eq!(v.current_piece_index(), 0);
    assert_eq!(v.current_offset(), 0);
    assert!(!v.is_finished());
}

#[test]
fn test_piece_hash_validator_init_with_zero_pieces() {
    let ctx = make_dctx(1024, 0);
    let ps = make_ps(1024, 0);
    let mut v = PieceHashValidator::new(ctx, ps, 0, 0, 1024);
    v.init();
    assert!(v.is_finished(), "Init with zero pieces should set finished");
}

// ── Saturated validation progress tests ───────────────────────────────

#[test]
fn test_piece_hash_validator_validate_chunk_advances() {
    let ctx = make_dctx(1_048_576, 3_145_728);
    let ps = make_ps(1_048_576, 3_145_728);
    let mut v = PieceHashValidator::new(ctx, ps, 3, 3_145_728, 1_048_576);

    v.validate_chunk();
    assert_eq!(v.current_piece_index(), 1);
    assert_eq!(v.current_offset(), 1_048_576);
    assert!(!v.is_finished());

    v.validate_chunk();
    assert_eq!(v.current_piece_index(), 2);
    assert_eq!(v.current_offset(), 2_097_152);
    assert!(!v.is_finished());
}

#[test]
fn test_piece_hash_validator_saturates_at_total_length() {
    let ctx = make_dctx(1_048_576, 2_097_152);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

    v.validate_chunk(); // piece 0 → piece 1
    v.validate_chunk(); // piece 1 → finished

    assert!(v.is_finished());
    // After finishing, offset should not exceed total_length
    assert!(v.current_offset() <= v.total_length());
}

// ── Finished flag management tests ────────────────────────────────────

#[test]
fn test_piece_hash_validator_finished_after_all_chunks() {
    let ctx = make_dctx(1_048_576, 2_097_152);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

    assert!(!v.is_finished());
    v.validate_chunk();
    assert!(!v.is_finished());
    v.validate_chunk();
    assert!(v.is_finished());
}

#[test]
fn test_piece_hash_validator_validate_after_finished_is_noop() {
    let ctx = make_dctx(1_048_576, 1_048_576);
    let ps = make_ps(1_048_576, 1_048_576);
    let mut v = PieceHashValidator::new(ctx, ps, 1, 1_048_576, 1_048_576);

    v.validate_chunk();
    assert!(v.is_finished());

    // Calling validate_chunk again should not panic or change state
    v.validate_chunk();
    assert!(v.is_finished());
    assert_eq!(v.current_piece_index(), 1);
}

// ── Validation result collection tests ─────────────────────────────────

#[test]
fn test_piece_hash_validator_collects_failed_results() {
    // No disk adaptor connected → all reads fail → all pieces marked failed
    let ctx = make_dctx(1_048_576, 2_097_152);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

    v.validate_chunk();
    v.validate_chunk();
    assert!(v.is_finished());

    // All pieces should have failed (no disk adaptor)
    let results = v.validation_results();
    assert_eq!(results.len(), 2);
    assert!(matches!(
        results[0],
        PieceValidationResult::Failed { piece_index: 0 }
    ));
    assert!(matches!(
        results[1],
        PieceValidationResult::Failed { piece_index: 1 }
    ));
    assert_eq!(v.pieces_failed(), 2);
    assert_eq!(v.pieces_ok(), 0);
}

#[test]
fn test_piece_hash_validator_apply_results() {
    let ctx = make_dctx(1_048_576, 2_097_152);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut v = PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576);

    v.validate_chunk();
    v.validate_chunk();

    // Apply results to a fresh PieceStorage
    let mut ps2 = crate::segment::piece_storage::DefaultPieceStorage::new(1_048_576, 2_097_152);
    v.apply_validation_results(&mut ps2);
    // Should not panic
}

// ── CheckIntegrityKind enum dispatch tests ────────────────────────────

#[test]
fn test_check_integrity_kind_stream() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let entry = CheckIntegrityKind::Stream(StreamCheckIntegrity::new(ctx, ps, false));
    assert!(!entry.is_validation_ready()); // No piece hashes set
    assert!(entry.is_finished()); // No validator yet (None), so finished
    assert_eq!(entry.total_length(), 0);
    assert_eq!(entry.current_length(), 0);
    assert!(entry.should_report_incomplete_as_error());
}

#[test]
fn test_check_integrity_kind_bt() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let entry = CheckIntegrityKind::Bt(BtCheckIntegrity::new(ctx, ps));
    assert!(!entry.is_validation_ready()); // No piece hashes set
    assert!(entry.is_finished()); // No validator yet (None), so finished
    assert_eq!(entry.total_length(), 0);
    assert_eq!(entry.current_length(), 0);
    assert!(!entry.should_report_incomplete_as_error());
}

#[test]
fn test_check_integrity_kind_init_and_validate_stream() {
    let ctx = make_dctx(1_048_576, 3_145_728);
    let ps = make_ps(1_048_576, 3_145_728);
    let mut entry = CheckIntegrityKind::Stream(StreamCheckIntegrity::new(ctx, ps, false));
    entry.init_validator();
    // Without piece hashes, the validator won't be created,
    // so it remains finished (ValidatorKind::None).
    assert!(entry.is_finished());
}

#[test]
fn test_check_integrity_kind_init_and_validate_bt() {
    let ctx = make_dctx(1_048_576, 2_097_152);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut entry = CheckIntegrityKind::Bt(BtCheckIntegrity::new(ctx, ps));
    entry.init_validator();
    // Without piece hashes, the validator won't be created,
    // so it remains finished (ValidatorKind::None).
    assert!(entry.is_finished());
}

// ── StreamCheckIntegrity creation and validation_ready tests ──────────

#[test]
fn test_stream_check_integrity_new() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let s = StreamCheckIntegrity::new(ctx, ps, false);
    assert!(!s.hash_check_only());
    assert!(s.is_finished()); // No validator → finished
}

#[test]
fn test_stream_check_integrity_hash_check_only() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let mut s = StreamCheckIntegrity::new(ctx, ps, true);
    assert!(s.hash_check_only());
    s.set_hash_check_only(false);
    assert!(!s.hash_check_only());
}

#[test]
fn test_stream_check_integrity_validation_ready() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let s = StreamCheckIntegrity::new(ctx, ps, false);
    // No piece hashes set → not ready
    assert!(!s.is_validation_ready());
}

#[test]
fn test_stream_check_integrity_validation_ready_with_hashes() {
    let mut ctx = crate::download::DownloadContext::new(1024, 4096, "/tmp/test.bin".to_string());
    ctx.set_piece_hashes(
        "sha-1".to_string(),
        vec![
            "h1".to_string(),
            "h2".to_string(),
            "h3".to_string(),
            "h4".to_string(),
        ],
    );
    let ctx = Arc::new(ctx);
    let ps = make_ps(1024, 4096);
    let s = StreamCheckIntegrity::new(ctx, ps, false);
    assert!(s.is_validation_ready());
}

#[test]
fn test_stream_check_integrity_init_validator() {
    let ctx = make_dctx(1_048_576, 4_194_304);
    let ps = make_ps(1_048_576, 4_194_304);
    let mut s = StreamCheckIntegrity::new(ctx, ps, false);
    assert!(s.is_finished()); // No validator yet

    // Without piece hashes, init_validator is a no-op (validator stays None)
    s.init_validator();
    assert!(s.is_finished()); // Still finished (no validator created)
}

#[test]
fn test_stream_check_integrity_init_validator_with_hashes() {
    let mut ctx =
        crate::download::DownloadContext::new(1_048_576, 4_194_304, "/tmp/test.bin".to_string());
    ctx.set_piece_hashes(
        "sha-1".to_string(),
        vec![
            "h1".to_string(),
            "h2".to_string(),
            "h3".to_string(),
            "h4".to_string(),
        ],
    );
    let ctx = Arc::new(ctx);
    let ps = make_ps(1_048_576, 4_194_304);
    let mut s = StreamCheckIntegrity::new(ctx, ps, false);
    assert!(s.is_finished()); // No validator yet

    s.init_validator();
    assert!(!s.is_finished()); // Validator created, not yet finished
    assert_eq!(s.total_length(), 4_194_304);
}

#[test]
fn test_stream_check_integrity_validator_access() {
    let ctx = make_dctx(1_048_576, 1_048_576);
    let ps = make_ps(1_048_576, 1_048_576);
    let mut s = StreamCheckIntegrity::new(ctx, ps, false);
    assert!(matches!(s.validator(), ValidatorKind::None));

    // Without piece hashes, init_validator is a no-op
    s.init_validator();
    assert!(matches!(s.validator(), ValidatorKind::None));
}

#[test]
fn test_stream_check_integrity_on_download_finished_noop() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let s = StreamCheckIntegrity::new(ctx, ps, false);
    assert_eq!(s.on_download_finished(), IntegrityFinishedAction::default());
}

#[test]
fn test_stream_check_integrity_on_download_incomplete() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let s = StreamCheckIntegrity::new(ctx, ps, false);
    let action = s.on_download_incomplete();
    assert!(action.reset_piece_storage);
    assert_eq!(action.file_allocation.as_ref().unwrap().len(), 1);
    assert_eq!(action.file_allocation.unwrap()[0].length, 4096);
}

#[test]
fn test_integrity_incomplete_action_applies_piece_storage_reset() {
    use crate::segment::piece_storage::{
        DefaultPieceStorage, PieceStorage, StreamPieceSelectorKind,
    };

    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let action = StreamCheckIntegrity::new(ctx, ps, false).on_download_incomplete();

    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.set_stream_piece_selector(StreamPieceSelectorKind::Geom);
    storage.set_bitfield(&[0b1100_0000]);
    storage.mark_piece_missing(0);

    // Leave the geometric selector stale, then apply the owner-side plan.
    // The reset must make the newly missing first piece selectable again.
    action.apply_piece_storage(&mut storage);

    let piece = storage
        .get_missing_piece(0, &[0], 0, 1)
        .expect("the reset selector should return the first missing piece");
    assert_eq!(piece.index(), 0);
}

#[test]
fn test_stream_check_integrity_hash_check_only_skips_allocation() {
    let ctx1 = make_dctx(1024, 4096);
    let ps1 = make_ps(1024, 4096);
    let ctx2 = make_dctx(1024, 4096);
    let ps2 = make_ps(1024, 4096);
    let s_with = StreamCheckIntegrity::new(ctx1, ps1, true);
    let s_without = StreamCheckIntegrity::new(ctx2, ps2, false);
    assert!(s_with.on_download_incomplete().file_allocation.is_none());
    assert!(s_without.on_download_incomplete().file_allocation.is_some());
}

// ── BtCheckIntegrity tests ────────────────────────────────────────────

#[test]
fn test_bt_check_integrity_new() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let b = BtCheckIntegrity::new(ctx, ps);
    assert!(b.is_finished()); // No validator yet
    assert!(!b.should_report_incomplete_as_error());
}

#[test]
fn test_bt_check_integrity_init_validator() {
    let mut ctx =
        crate::download::DownloadContext::new(1_048_576, 2_097_152, "/tmp/test.bin".to_string());
    ctx.set_piece_hashes(
        "sha-1".to_string(),
        vec!["h1".to_string(), "h2".to_string()],
    );
    let ctx = Arc::new(ctx);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut b = BtCheckIntegrity::new(ctx, ps);
    b.init_validator();
    assert!(!b.is_finished());
    assert_eq!(b.total_length(), 2_097_152);
}

#[test]
fn test_bt_check_integrity_on_download_handlers() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let b = BtCheckIntegrity::new(ctx, ps);
    assert!(b.on_download_finished().file_allocation.is_some());
    let incomplete = b.on_download_incomplete();
    assert!(incomplete.reset_piece_storage);
    assert!(incomplete.file_allocation.is_some());
}

#[test]
fn test_bt_check_integrity_options_gate_dispatch() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let b = BtCheckIntegrity::new_with_options(ctx, ps, true, false, true);

    let finished = b.on_download_finished();
    assert!(finished.file_allocation.is_none());
    assert!(finished.run_completion_hook);
    assert!(b.on_download_incomplete().file_allocation.is_none());
}

#[test]
fn test_integrity_callback_trailing_garbage_lists_physical_files() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let stream = StreamCheckIntegrity::new(ctx, ps, false);

    let action = stream.cut_trailing_garbage();
    assert_eq!(action.files.len(), 1);
    assert_eq!(
        action.files[0].path.to_string_lossy(),
        "/tmp/test_check_integrity.bin"
    );
    assert_eq!(action.files[0].length, 4096);
}

#[tokio::test]
async fn test_integrity_callback_trailing_garbage_applies_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("payload.bin");
    tokio::fs::write(&path, vec![0u8; 12]).await.unwrap();
    let ctx = Arc::new(crate::download::DownloadContext::new(
        1024,
        8,
        path.to_string_lossy().into_owned(),
    ));
    let ps = make_ps(1024, 8);
    let action = StreamCheckIntegrity::new(ctx, ps, false).cut_trailing_garbage();

    action.apply().await.unwrap();

    assert_eq!(tokio::fs::metadata(path).await.unwrap().len(), 8);
}

// ── Cross-cutting: ValidatorKind after PieceHashValidator assignment ───

#[test]
fn test_validator_kind_piece_hash_full_lifecycle() {
    let ctx = make_dctx(1_048_576, 2_097_152);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut v = ValidatorKind::PieceHash(PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576));

    v.init();
    assert!(!v.is_finished());
    assert_eq!(v.current_offset(), 0);

    v.validate_chunk();
    assert_eq!(v.current_offset(), 1_048_576);

    v.validate_chunk();
    assert!(v.is_finished());
    assert_eq!(v.current_offset(), 2_097_152);

    // Validate after finish should be no-op
    v.validate_chunk();
    assert!(v.is_finished());
}

#[test]
fn test_validator_kind_init_on_piece_hash() {
    let ctx = make_dctx(1_048_576, 3_145_728);
    let ps = make_ps(1_048_576, 3_145_728);
    let mut v = ValidatorKind::PieceHash(PieceHashValidator::new(ctx, ps, 3, 3_145_728, 1_048_576));
    v.validate_chunk(); // advance to piece 1
    assert_eq!(v.current_offset(), 1_048_576);

    v.init(); // reset
    assert_eq!(v.current_offset(), 0);
    assert!(!v.is_finished());
}

#[test]
fn test_validator_kind_apply_validation_results() {
    let ctx = make_dctx(1_048_576, 2_097_152);
    let ps = make_ps(1_048_576, 2_097_152);
    let mut v = ValidatorKind::PieceHash(PieceHashValidator::new(ctx, ps, 2, 2_097_152, 1_048_576));

    v.validate_chunk();
    v.validate_chunk();
    assert!(v.is_finished());

    // Apply results to a fresh PieceStorage
    let mut ps2 = crate::segment::piece_storage::DefaultPieceStorage::new(1_048_576, 2_097_152);
    v.apply_validation_results(&mut ps2);
    // Should not panic
}

#[test]
fn test_validator_kind_apply_validation_results_none_variant() {
    let v = ValidatorKind::None;
    let mut ps = crate::segment::piece_storage::DefaultPieceStorage::new(1_048_576, 2_097_152);
    // Should not panic — no-op for None variant
    v.apply_validation_results(&mut ps);
}

// ── Debug trait tests ──────────────────────────────────────────────────

#[test]
fn test_stream_check_integrity_debug() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let s = StreamCheckIntegrity::new(ctx, ps, false);
    let debug_str = format!("{:?}", s);
    assert!(debug_str.contains("StreamCheckIntegrity"));
    assert!(debug_str.contains("hash_check_only: false"));
}

#[test]
fn test_bt_check_integrity_debug() {
    let ctx = make_dctx(1024, 4096);
    let ps = make_ps(1024, 4096);
    let b = BtCheckIntegrity::new(ctx, ps);
    let debug_str = format!("{:?}", b);
    assert!(debug_str.contains("BtCheckIntegrity"));
}

#[test]
fn test_piece_validation_result_debug() {
    let r1 = PieceValidationResult::Verified { piece_index: 0 };
    let r2 = PieceValidationResult::Failed { piece_index: 1 };
    assert!(format!("{:?}", r1).contains("Verified"));
    assert!(format!("{:?}", r2).contains("Failed"));
}

// ── Piece hash algorithm selection ────────────────────────────────────
//
// `compute_hash` used to hardcode SHA-1 while C++
// `IteratableChunkChecksumValidator` builds its MessageDigest from
// `dctx->getPieceHashType()`. Any download whose piece hashes were not SHA-1
// (Metalink `<pieces type="sha-256">`, HTTP piece hashes) therefore compared a
// 40-char digest against a 64-char expected value and failed every piece.

/// Build a validator whose DownloadContext declares `hash_type` piece hashes.
fn validator_with_hash_type(hash_type: &str) -> PieceHashValidator {
    let mut ctx = crate::download::DownloadContext::new(4, 8, "/tmp/hash_type.bin".to_string());
    if !hash_type.is_empty() {
        ctx.set_piece_hashes(
            hash_type.to_string(),
            vec!["x".to_string(), "y".to_string()],
        );
    }
    PieceHashValidator::new(Arc::new(ctx), make_ps(4, 8), 2, 8, 4)
}

#[test]
fn test_compute_hash_honours_sha256_piece_hash_type() {
    let v = validator_with_hash_type("sha-256");
    let got = v.compute_hash(b"abc").expect("sha-256 must be supported");
    assert_eq!(
        got, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "must hash with SHA-256, not SHA-1"
    );
}

#[test]
fn test_compute_hash_honours_sha512_piece_hash_type() {
    let v = validator_with_hash_type("sha-512");
    let got = v.compute_hash(b"abc").expect("sha-512 must be supported");
    assert_eq!(got.len(), 128, "SHA-512 hex digest is 128 chars");
}

#[test]
fn test_compute_hash_honours_md5_piece_hash_type() {
    let v = validator_with_hash_type("md5");
    let got = v.compute_hash(b"abc").expect("md5 must be supported");
    assert_eq!(got, "900150983cd24fb0d6963f7d28e17f72");
}

#[test]
fn test_compute_hash_defaults_to_sha1_when_type_empty() {
    // Empty piece hash type means BitTorrent, whose piece hashes are SHA-1.
    let v = validator_with_hash_type("");
    assert_eq!(v.resolve_hash_type(), Some(HashType::Sha1));
    let got = v.compute_hash(b"abc").expect("sha-1 fallback");
    assert_eq!(got, "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn test_compute_hash_rejects_unknown_algorithm() {
    let v = validator_with_hash_type("not-a-real-hash");
    assert_eq!(v.resolve_hash_type(), None);
    assert!(
        v.compute_hash(b"abc").is_none(),
        "an unrecognised algorithm must not silently fall back to SHA-1"
    );
}
