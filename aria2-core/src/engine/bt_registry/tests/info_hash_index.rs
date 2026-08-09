//! Unit tests for BtRegistry — info_hash secondary index.

use super::super::*;
use std::sync::Arc;

use crate::download::DownloadContext;

/// Helper: create a BtObject with DownloadContext and TorrentAttribute.
fn make_bt_object_with_info_hash(
    piece_length: u32,
    total_length: u64,
    path: &str,
    info_hash: &str,
    private: bool,
) -> BtObject {
    use crate::download::download_context::{ContextAttributeType, TorrentAttribute};

    let mut ctx = DownloadContext::new(piece_length, total_length, path.to_string());
    let mut ta = TorrentAttribute::new(info_hash.to_string());
    ta.private_torrent = private;
    ta.name = format!("torrent_{}", info_hash);
    ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(ta));
    let ctx = Arc::new(ctx);

    BtObject {
        download_context: Some(ctx),
        ..BtObject::new()
    }
}

/// Helper: create a BtObject with only a DownloadContext.
fn make_bt_object_with_ctx(piece_length: u32, total_length: u64, path: &str) -> BtObject {
    let ctx = Arc::new(DownloadContext::new(
        piece_length,
        total_length,
        path.to_string(),
    ));
    BtObject {
        download_context: Some(ctx),
        ..BtObject::new()
    }
}

#[test]
fn test_get_download_context_by_info_hash_returns_none_without_attribute() {
    let mut registry = BtRegistry::new();
    let obj = make_bt_object_with_ctx(1024, 4096, "/tmp/file.bin");
    registry.put(1, obj);

    assert!(
        registry
            .get_download_context_by_info_hash("any_hash")
            .is_none()
    );
}

#[test]
fn test_get_download_context_by_info_hash_with_torrent_attribute() {
    let info_hash = "0123456789abcdef0123456789abcdef01234567";
    let obj = make_bt_object_with_info_hash(1024, 4096, "/tmp/file.bin", info_hash, false);

    let mut registry = BtRegistry::new();
    registry.put(1, obj);

    let found = registry.get_download_context_by_info_hash(info_hash);
    assert!(found.is_some());

    assert!(
        registry
            .get_download_context_by_info_hash("wrong_hash")
            .is_none()
    );

    assert_eq!(registry.info_hash_index_len(), 1);
}

#[test]
fn test_info_hash_index_populated_on_put() {
    let hash1 = "aaa1111111111111111111111111111111111111";
    let hash2 = "bbb2222222222222222222222222222222222222";

    let mut registry = BtRegistry::new();
    let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
    let obj2 = make_bt_object_with_info_hash(2048, 8192, "/tmp/b.bin", hash2, false);

    registry.put(1, obj1);
    registry.put(2, obj2);

    assert_eq!(registry.info_hash_index_len(), 2);

    let ctx = registry.get_download_context_by_info_hash(hash1);
    assert!(ctx.is_some());

    let ctx = registry.get_download_context_by_info_hash(hash2);
    assert!(ctx.is_some());
}

#[test]
fn test_info_hash_index_cleaned_on_remove() {
    let hash1 = "aaa1111111111111111111111111111111111111";
    let mut registry = BtRegistry::new();
    let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
    registry.put(1, obj1);

    assert_eq!(registry.info_hash_index_len(), 1);
    assert!(registry.remove(1));
    assert_eq!(registry.info_hash_index_len(), 0);

    assert!(registry.get_download_context_by_info_hash(hash1).is_none());
}

#[test]
fn test_info_hash_index_cleaned_on_overwrite() {
    let hash1 = "aaa1111111111111111111111111111111111111";
    let hash2 = "bbb2222222222222222222222222222222222222";

    let mut registry = BtRegistry::new();
    let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
    registry.put(1, obj1);
    assert_eq!(registry.info_hash_index_len(), 1);

    let obj2 = make_bt_object_with_info_hash(2048, 8192, "/tmp/b.bin", hash2, false);
    registry.put(1, obj2);

    assert_eq!(registry.info_hash_index_len(), 1);
    assert!(registry.get_download_context_by_info_hash(hash1).is_none());
    assert!(registry.get_download_context_by_info_hash(hash2).is_some());
}

#[test]
fn test_info_hash_index_cleared_on_remove_all() {
    let hash1 = "aaa1111111111111111111111111111111111111";
    let mut registry = BtRegistry::new();
    let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
    registry.put(1, obj1);

    assert_eq!(registry.info_hash_index_len(), 1);
    registry.remove_all();
    assert_eq!(registry.info_hash_index_len(), 0);
}

#[test]
fn test_rebuild_info_hash_index() {
    let mut registry = BtRegistry::new();
    let obj1 = make_bt_object_with_info_hash(
        1024,
        4096,
        "/tmp/a.bin",
        "aaa1111111111111111111111111111111111111",
        false,
    );
    registry.pool.insert(1, obj1);

    assert_eq!(registry.info_hash_index_len(), 0);

    registry.rebuild_info_hash_index();
    assert_eq!(registry.info_hash_index_len(), 1);
    assert!(
        registry
            .get_download_context_by_info_hash("aaa1111111111111111111111111111111111111")
            .is_some()
    );
}

#[test]
fn test_info_hash_index_last_writer_wins() {
    let hash = "aaa1111111111111111111111111111111111111";

    let mut registry = BtRegistry::new();

    let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash, false);
    registry.put(1, obj1);
    assert_eq!(registry.info_hash_index.get(hash), Some(&1));

    let obj2 = make_bt_object_with_info_hash(2048, 8192, "/tmp/b.bin", hash, false);
    registry.put(2, obj2);
    assert_eq!(registry.info_hash_index.get(hash), Some(&2));

    let ctx = registry.get_download_context_by_info_hash(hash);
    assert!(ctx.is_some());
    assert_eq!(ctx.unwrap().get_piece_length(), 2048);

    assert!(registry.get(1).is_some());
}
